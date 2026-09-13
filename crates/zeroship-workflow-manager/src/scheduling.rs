//! Manager-owned calendar frontiers and activation-gated occurrence delivery.
#![expect(
    clippy::future_not_send,
    reason = "scheduling uses the manager's compio-local ORM"
)]

use crate::{
    models::schema::{
        schedule_activations, schedule_deployments, schedule_occurrences, schedule_scopes,
        schedules,
    },
    queue, recovery, Error, Queue,
};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{RequestId, Revision, RunId},
    workflow_jobs::{DeploymentId, JobId, JobOperation, JobSpec},
    workflow_schedules::{ActivateSchedules, RegisterSchedules, ScheduleDescriptor, ScheduleId},
};
use zeroship_data_orm::{
    orm::{Database, Entity, FindOptions, FromRow, Operation, Output},
    value,
};
use zeroship_workflow_calendar::{ScheduleCatchUp, ScheduleTiming};

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub max_schedules: usize,
    pub max_backfill: usize,
    pub min_interval_ms: i64,
    pub page_size: u32,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            max_schedules: 64,
            max_backfill: 32,
            min_interval_ms: 1_000,
            page_size: 128,
        }
    }
}

/// The platform host supplies verified deployment metadata and activation order.
/// No creator database or worker registration is required to produce due jobs.
#[derive(Clone, Debug)]
pub struct Scheduler {
    queue: Queue,
    options: Options,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueSchedule {
    pub app_id: AppId,
    pub schedule_id: ScheduleId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub jobs: Vec<JobSpec>,
    /// The captured catch-up boundary still needs another processing page.
    pub more: bool,
}

#[derive(FromRow)]
#[orm(entity = schedule_deployments)]
struct Prepared {
    definition: String,
    interpretation: String,
}

#[derive(FromRow)]
#[orm(entity = schedule_activations)]
struct Activation {
    id: String,
    deployment_id: String,
    revision: i64,
    activated_at: i64,
}

#[derive(FromRow)]
#[orm(entity = schedule_scopes)]
struct Active {
    revision: i64,
    activation_id: String,
}

#[derive(FromRow)]
#[orm(entity = schedules)]
struct Schedule {
    id: String,
    name: String,
    activation_id: String,
    revision: i64,
    definition: String,
    next_at: Option<i64>,
    anchor_at: i64,
    catch_up_until: Option<i64>,
    catch_up_remaining: Option<i64>,
}

#[derive(FromRow)]
#[orm(entity = schedules)]
struct Due {
    id: String,
    app_id: String,
}

#[derive(FromRow)]
#[orm(entity = schedule_occurrences)]
struct Occurrence {
    id: String,
    schedule_id: String,
    revision: i64,
    scheduled_at: i64,
    run_id: String,
    job_id: String,
    activation_id: String,
}

impl Scheduler {
    /// # Errors
    /// Refuses empty bounds and values outside the ORM's portable range.
    pub fn new(queue: Queue, options: Options) -> Result<Self, Error> {
        if options.max_schedules == 0
            || options.max_backfill == 0
            || i64::try_from(options.max_backfill).is_err()
            || options.min_interval_ms <= 0
            || options.page_size == 0
            || i64::from(options.page_size) > zeroship_data_orm::sql::MAX_ROW_LIMIT
        {
            return Err(Error::Invalid);
        }
        Ok(Self { queue, options })
    }

    /// Persist an immutable, input-free projection of a verified app deployment.
    /// Reordering its schedule list is harmless; changing any descriptor conflicts.
    ///
    /// # Errors
    /// Rejects invalid metadata, reused deployment identities and storage failures.
    pub async fn prepare(&self, request: &RegisterSchedules) -> Result<(), Error> {
        let mut request = request.clone();
        request.schedules.sort_by(|a, b| a.name.cmp(&b.name));
        self.validate(&request)?;
        let definition =
            String::from_utf8(self.queue.encode(&request)?).map_err(|_| Error::Invalid)?;
        self.queue.transact(|tx| async move {
            queue::register_scope_in(&tx, &request.app_id).await?;
            queue::lock_scope(&tx, &request.app_id).await?;
            if let Some(existing) = prepared(&tx, &request.app_id, &request.deployment_id).await? {
                return if existing.definition == definition { Ok(()) } else { Err(Error::Conflict) };
            }
            tx.collection(schedule_deployments::Entity::COLLECTION)?.insert(value!({
                "id":request.deployment_id.as_str(), "app_id":request.app_id.as_str(),
                "definition":definition, "interpretation":zeroship_workflow_calendar::interpretation(),
                "created_at":self.queue.clock.now().await?,
            })).await?;
            Ok(())
        }).await
    }

    fn validate(&self, request: &RegisterSchedules) -> Result<(), Error> {
        if request.schedules.len() > self.options.max_schedules {
            return Err(Error::Capacity);
        }
        let mut previous: Option<&str> = None;
        for descriptor in &request.schedules {
            for name in [&descriptor.name, &descriptor.workflow_name] {
                if name.is_empty() || name.len() > 128 || name.starts_with("__zs.") {
                    return Err(Error::Invalid);
                }
            }
            if previous == Some(descriptor.name.as_str()) {
                return Err(Error::Conflict);
            }
            previous = Some(&descriptor.name);
            if let ScheduleCatchUp::Backfill { max } = descriptor.catch_up {
                if max == 0 || max > self.options.max_backfill {
                    return Err(Error::Capacity);
                }
            }
            if let ScheduleTiming::Interval { interval_ms, .. } = descriptor.schedule {
                if interval_ms < self.options.min_interval_ms {
                    return Err(Error::Invalid);
                }
            }
            descriptor
                .schedule
                .next_after(0, 0)
                .map_err(|_| Error::Invalid)?;
        }
        Ok(())
    }

    /// Select future scheduling and enqueue activation in the same transaction.
    /// Previously created occurrences keep their own activation prerequisite and pin.
    /// Matching old requests replay without changing the current activation.
    ///
    /// # Errors
    /// Rejects unprepared deployments, stale/conflicting revisions, changed calendar
    /// interpretation and failed storage. Preparation alone grants no activation.
    pub async fn activate(&self, request: &ActivateSchedules) -> Result<JobSpec, Error> {
        self.queue.transact(|tx| async move {
            queue::lock_scope(&tx, &request.app_id).await?;
            let existing = tx.entity::<schedule_activations::Entity>()?.find::<Activation>(
                schedule_activations::app_id.eq(request.app_id.as_str())?
                    .and(schedule_activations::revision.eq(request.revision.get())?),
                one(),
            ).await?.into_iter().next();
            if let Some(existing) = existing {
                if existing.deployment_id != request.deployment_id.as_str() { return Err(Error::Conflict); }
                return activation_job(&tx, &request.app_id, &existing).await?.spec();
            }
            let active = active(&tx, &request.app_id).await?;
            if active.as_ref().is_some_and(|active| active.revision >= request.revision.get()) {
                return Err(Error::Conflict);
            }
            let prepared = prepared(&tx, &request.app_id, &request.deployment_id).await?.ok_or(Error::Denied)?;
            check_interpretation(&prepared)?;
            let metadata = metadata(&prepared, &request.app_id, &request.deployment_id)?;
            self.validate(&metadata)?;
            let now = self.queue.clock.now().await?;
            let job = JobSpec {
                id: JobId::mint(), app_id: request.app_id.clone(), deployment_id: request.deployment_id.clone(),
                operation: JobOperation::Activate { revision: request.revision },
                available_at: now.try_into().map_err(|_| Error::Storage)?,
            };
            self.queue.insert(&tx, &job, now).await?;
            tx.collection(schedule_activations::Entity::COLLECTION)?.insert(value!({
                "id":job.id.as_str(), "app_id":request.app_id.as_str(),
                "deployment_id":request.deployment_id.as_str(), "revision":request.revision.get(), "activated_at":now,
            })).await?;
            if let Some(active) = active {
                changed(&tx.collection(schedule_scopes::Entity::COLLECTION)?.execute(Operation::Update {
                    filter:value!({"id":request.app_id.as_str(), "revision":active.revision, "activation_id":active.activation_id}),
                    patch:value!({"revision":request.revision.get(), "activation_id":job.id.as_str()}), many:true,
                }).await?)?;
            } else {
                tx.collection(schedule_scopes::Entity::COLLECTION)?.insert(value!({
                    "id":request.app_id.as_str(), "revision":request.revision.get(), "activation_id":job.id.as_str(),
                })).await?;
            }
            tx.collection(schedules::Entity::COLLECTION)?.execute(Operation::Update {
                filter:value!({"app_id":request.app_id.as_str()}),
                patch:value!({"next_at":null, "catch_up_until":null, "catch_up_remaining":null}), many:true,
            }).await?;
            for descriptor in &metadata.schedules {
                self.install(&tx, request, &job.id, descriptor, now).await?;
            }
            recovery::ensure_in(&tx, &request.app_id, &request.deployment_id, request.revision, now).await?;
            Ok(job)
        }).await
    }

    async fn install(
        &self,
        tx: &Database,
        activation: &ActivateSchedules,
        job: &JobId,
        descriptor: &ScheduleDescriptor,
        now: i64,
    ) -> Result<(), Error> {
        let previous = tx
            .entity::<schedules::Entity>()?
            .find::<Schedule>(
                schedules::app_id
                    .eq(activation.app_id.as_str())?
                    .and(schedules::name.eq(descriptor.name.as_str())?),
                one(),
            )
            .await?
            .into_iter()
            .next();
        let definition = serde_json::to_string(descriptor).map_err(|_| Error::Invalid)?;
        let next = descriptor
            .schedule
            .next_after(now, now)
            .map_err(|_| Error::Invalid)?;
        let mut document = value!({
            "activation_id":job.as_str(), "revision":activation.revision.get(), "definition":definition,
            "next_at":next, "anchor_at":now, "catch_up_until":null, "catch_up_remaining":null,
        });
        let collection = tx.collection(schedules::Entity::COLLECTION)?;
        if let Some(previous) = previous {
            changed(
                &collection
                    .execute(Operation::Update {
                        filter: value!({"id":previous.id, "app_id":activation.app_id.as_str()}),
                        patch: document,
                        many: true,
                    })
                    .await?,
            )?;
        } else {
            let fields = document.as_object_mut().ok_or(Error::Storage)?;
            fields.insert("id".into(), value!(ScheduleId::mint().as_str()));
            fields.insert("app_id".into(), value!(activation.app_id.as_str()));
            fields.insert("name".into(), value!(descriptor.name));
            collection.insert(document).await?;
        }
        Ok(())
    }

    /// Page due schedules across app scopes using the manager database clock.
    /// Restart from the beginning after an empty page to revisit work behind the cursor.
    ///
    /// # Errors
    /// Reports malformed stored identities or unavailable manager storage.
    pub async fn due(&self, after: Option<&ScheduleId>) -> Result<Vec<DueSchedule>, Error> {
        self.queue
            .transact(|tx| async move {
                let source = tx.entity::<schedules::Entity>()?.alias("s")?;
                let mut filters = vec![source
                    .column(schedules::next_at)
                    .lte(Some(self.queue.clock.now().await?))?];
                if let Some(after) = after {
                    filters.push(source.column(schedules::id).gt(after.as_str())?);
                }
                tx.from(&source)
                    .filter(zeroship_data_orm::sql::Predicate::And(filters))
                    .order_by(source.column(schedules::id).asc())
                    .select(source.row::<Due>())?
                    .limit(i64::from(self.options.page_size))?
                    .all()
                    .await?
                    .into_iter()
                    .map(|row| {
                        Ok(DueSchedule {
                            app_id: AppId::parse(&row.app_id).map_err(|_| Error::Storage)?,
                            schedule_id: ScheduleId::parse(&row.id).map_err(|_| Error::Storage)?,
                        })
                    })
                    .collect()
            })
            .await
    }

    /// Commit a bounded occurrence page, its stable jobs and the calendar cursor.
    /// The catch-up boundary and remaining semantic allowance survive page limits,
    /// replicas and restart. Queue claims wait for each occurrence's activation job.
    ///
    /// # Errors
    /// Rejects a foreign/missing schedule, changed interpretation and storage failures.
    /// Failure preserves the previous cursor; retry cannot mint another committed occurrence.
    pub async fn dispatch(&self, app: &AppId, id: &ScheduleId) -> Result<Page, Error> {
        self.queue.transact(|tx| async move {
            queue::lock_scope(&tx, app).await?;
            let record = schedule(&tx, app, id).await?.ok_or(Error::Denied)?;
            let now = self.queue.clock.now().await?;
            let Some(mut at) = record.next_at.filter(|at| *at <= now) else { return Ok(Page { jobs:vec![], more:false }); };
            let activation = load_activation(&tx, app, &record.activation_id).await?;
            if activation.revision != record.revision { return Err(Error::Storage); }
            activation_job(&tx, app, &activation).await?;
            let deployment = DeploymentId::parse(&activation.deployment_id).map_err(|_| Error::Storage)?;
            let prepared = prepared(&tx, app, &deployment).await?.ok_or(Error::Storage)?;
            check_interpretation(&prepared)?;
            let metadata = metadata(&prepared, app, &deployment)?;
            let definition: ScheduleDescriptor = serde_json::from_str(&record.definition).map_err(|_| Error::Storage)?;
            if definition.name != record.name || descriptor(&metadata, &record.name)? != &definition { return Err(Error::Storage); }
            let allowance = match definition.catch_up {
                ScheduleCatchUp::Skip => 1,
                ScheduleCatchUp::Backfill { max } => i64::try_from(max).map_err(|_| Error::Storage)?,
            };
            let (boundary, mut remaining) = match (record.catch_up_until, record.catch_up_remaining) {
                (Some(boundary), Some(remaining)) if boundary >= at && remaining > 0 && remaining <= allowance => (boundary, remaining),
                (None, None) if allowance > 0 => (now, allowance.min(i64::try_from(self.options.max_backfill).map_err(|_| Error::Invalid)?)),
                _ => return Err(Error::Storage),
            };
            let mut jobs = Vec::new();
            while at <= boundary && remaining > 0 && jobs.len() < self.options.page_size as usize {
                let job = self.occurrence(&tx, app, id, &record, &definition, &deployment, at, now).await?;
                jobs.push(job);
                at = definition.schedule.next_after(at, record.anchor_at).map_err(|_| Error::Storage)?;
                remaining -= 1;
            }
            if remaining == 0 && at <= boundary {
                at = definition.schedule.next_after(boundary, record.anchor_at).map_err(|_| Error::Storage)?;
            }
            let more = at <= boundary;
            changed(&tx.collection(schedules::Entity::COLLECTION)?.execute(Operation::Update {
                filter:value!({"id":id.as_str(), "app_id":app.as_str(), "activation_id":record.activation_id, "revision":record.revision}),
                patch:value!({"next_at":at, "catch_up_until":more.then_some(boundary), "catch_up_remaining":more.then_some(remaining)}), many:true,
            }).await?)?;
            Ok(Page { jobs, more })
        }).await
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the occurrence is bound to the captured schedule and activation"
    )]
    async fn occurrence(
        &self,
        tx: &Database,
        app: &AppId,
        id: &ScheduleId,
        record: &Schedule,
        definition: &ScheduleDescriptor,
        deployment: &DeploymentId,
        at: i64,
        now: i64,
    ) -> Result<JobSpec, Error> {
        let existing = tx
            .entity::<schedule_occurrences::Entity>()?
            .find::<Occurrence>(
                schedule_occurrences::app_id
                    .eq(app.as_str())?
                    .and(schedule_occurrences::schedule_id.eq(id.as_str())?)
                    .and(schedule_occurrences::revision.eq(record.revision)?)
                    .and(schedule_occurrences::scheduled_at.eq(at)?),
                one(),
            )
            .await?
            .into_iter()
            .next();
        if let Some(existing) = existing {
            if existing.activation_id != record.activation_id {
                return Err(Error::Storage);
            }
            let job = queue::load(tx, app, &existing.job_id)
                .await?
                .ok_or(Error::Storage)?
                .spec()?;
            check_occurrence(&existing, &definition.name, deployment, &job)?;
            return Ok(job);
        }
        let request = RequestId::mint();
        let run = RunId::mint();
        let job = JobSpec {
            id: JobId::mint(),
            app_id: app.clone(),
            deployment_id: deployment.clone(),
            operation: JobOperation::Cron {
                schedule_id: id.clone(),
                schedule_name: definition.name.clone(),
                request_id: request.clone(),
                run_id: run.clone(),
                revision: Revision::try_from(record.revision).map_err(|_| Error::Storage)?,
                scheduled_at: at.try_into().map_err(|_| Error::Storage)?,
            },
            available_at: at.try_into().map_err(|_| Error::Storage)?,
        };
        self.queue.insert(tx, &job, now).await?;
        tx.collection(schedule_occurrences::Entity::COLLECTION)?.insert(value!({
            "id":request.as_str(), "app_id":app.as_str(), "schedule_id":id.as_str(), "revision":record.revision,
            "scheduled_at":at, "run_id":run.as_str(), "job_id":job.id.as_str(), "activation_id":record.activation_id,
        })).await?;
        Ok(job)
    }
}

fn metadata(
    prepared: &Prepared,
    app: &AppId,
    deployment: &DeploymentId,
) -> Result<RegisterSchedules, Error> {
    let metadata: RegisterSchedules =
        serde_json::from_str(&prepared.definition).map_err(|_| Error::Storage)?;
    if &metadata.app_id != app || &metadata.deployment_id != deployment {
        return Err(Error::Storage);
    }
    Ok(metadata)
}

fn descriptor<'a>(
    metadata: &'a RegisterSchedules,
    name: &str,
) -> Result<&'a ScheduleDescriptor, Error> {
    let mut matches = metadata
        .schedules
        .iter()
        .filter(|definition| definition.name == name);
    let definition = matches.next().ok_or(Error::Storage)?;
    if matches.next().is_some() {
        return Err(Error::Storage);
    }
    Ok(definition)
}

async fn load_activation(tx: &Database, app: &AppId, id: &str) -> Result<Activation, Error> {
    tx.entity::<schedule_activations::Entity>()?
        .find::<Activation>(
            schedule_activations::app_id
                .eq(app.as_str())?
                .and(schedule_activations::id.eq(id)?),
            one(),
        )
        .await?
        .into_iter()
        .next()
        .ok_or(Error::Storage)
}

async fn activation_job(
    tx: &Database,
    app: &AppId,
    activation: &Activation,
) -> Result<crate::models::Job, Error> {
    let job = queue::load(tx, app, &activation.id)
        .await?
        .ok_or(Error::Storage)?;
    let expected = JobSpec {
        id: JobId::parse(&activation.id).map_err(|_| Error::Storage)?,
        app_id: app.clone(),
        deployment_id: DeploymentId::parse(&activation.deployment_id)
            .map_err(|_| Error::Storage)?,
        operation: JobOperation::Activate {
            revision: activation.revision.try_into().map_err(|_| Error::Storage)?,
        },
        available_at: activation
            .activated_at
            .try_into()
            .map_err(|_| Error::Storage)?,
    };
    if job.spec()? != expected {
        return Err(Error::Storage);
    }
    Ok(job)
}

fn check_occurrence(
    occurrence: &Occurrence,
    name: &str,
    deployment: &DeploymentId,
    job: &JobSpec,
) -> Result<(), Error> {
    let operation = JobOperation::Cron {
        schedule_id: ScheduleId::parse(&occurrence.schedule_id).map_err(|_| Error::Storage)?,
        schedule_name: name.to_owned(),
        request_id: RequestId::parse(&occurrence.id).map_err(|_| Error::Storage)?,
        run_id: RunId::parse(&occurrence.run_id).map_err(|_| Error::Storage)?,
        revision: occurrence.revision.try_into().map_err(|_| Error::Storage)?,
        scheduled_at: occurrence
            .scheduled_at
            .try_into()
            .map_err(|_| Error::Storage)?,
    };
    if occurrence.job_id != job.id.as_str()
        || job.operation != operation
        || &job.deployment_id != deployment
        || job.available_at.get() != occurrence.scheduled_at
    {
        return Err(Error::Storage);
    }
    Ok(())
}

/// The candidate join orders valid work, but a missing occurrence must never turn
/// a cron job into ordinary work. Check its immutable linkage before leasing.
pub(crate) async fn validate_delivery(
    tx: &Database,
    job: &crate::models::Job,
) -> Result<(), Error> {
    use zeroship_core::workflow_jobs::JobOutcome;
    let spec = job.spec()?;
    match &spec.operation {
        JobOperation::Activate { .. } => {
            let activation = load_activation(tx, &spec.app_id, &job.id).await?;
            activation_job(tx, &spec.app_id, &activation).await?;
        }
        JobOperation::Cron { schedule_id, .. } => {
            let occurrence = tx
                .entity::<schedule_occurrences::Entity>()?
                .find::<Occurrence>(
                    schedule_occurrences::app_id
                        .eq(spec.app_id.as_str())?
                        .and(schedule_occurrences::job_id.eq(job.id.as_str())?),
                    one(),
                )
                .await?
                .into_iter()
                .next()
                .ok_or(Error::Storage)?;
            let activation = load_activation(tx, &spec.app_id, &occurrence.activation_id).await?;
            if occurrence.revision != activation.revision {
                return Err(Error::Storage);
            }
            let prerequisite = activation_job(tx, &spec.app_id, &activation).await?;
            if prerequisite.state != "settled"
                || prerequisite.outcome.as_deref()
                    != Some(
                        &serde_json::to_string(&JobOutcome::Completed)
                            .map_err(|_| Error::Storage)?,
                    )
            {
                return Err(Error::Storage);
            }
            let deployment =
                DeploymentId::parse(&activation.deployment_id).map_err(|_| Error::Storage)?;
            let prepared = prepared(tx, &spec.app_id, &deployment)
                .await?
                .ok_or(Error::Storage)?;
            let metadata = metadata(&prepared, &spec.app_id, &deployment)?;
            let schedule = schedule(tx, &spec.app_id, schedule_id)
                .await?
                .ok_or(Error::Storage)?;
            let definition = descriptor(&metadata, &schedule.name)?;
            check_occurrence(&occurrence, &definition.name, &deployment, &spec)?;
        }
        _ => {}
    }
    Ok(())
}

fn check_interpretation(prepared: &Prepared) -> Result<(), Error> {
    if prepared.interpretation == zeroship_workflow_calendar::interpretation() {
        Ok(())
    } else {
        Err(Error::Conflict)
    }
}

async fn prepared(
    tx: &Database,
    app: &AppId,
    deployment: &DeploymentId,
) -> Result<Option<Prepared>, Error> {
    Ok(tx
        .entity::<schedule_deployments::Entity>()?
        .find::<Prepared>(
            schedule_deployments::app_id
                .eq(app.as_str())?
                .and(schedule_deployments::id.eq(deployment.as_str())?),
            one(),
        )
        .await?
        .into_iter()
        .next())
}

async fn active(tx: &Database, app: &AppId) -> Result<Option<Active>, Error> {
    Ok(tx
        .entity::<schedule_scopes::Entity>()?
        .find::<Active>(schedule_scopes::id.eq(app.as_str())?, one())
        .await?
        .into_iter()
        .next())
}

async fn schedule(tx: &Database, app: &AppId, id: &ScheduleId) -> Result<Option<Schedule>, Error> {
    Ok(tx
        .entity::<schedules::Entity>()?
        .find::<Schedule>(
            schedules::app_id
                .eq(app.as_str())?
                .and(schedules::id.eq(id.as_str())?),
            one(),
        )
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
const fn changed(output: &Output) -> Result<(), Error> {
    if matches!(output, Output::Count(1)) {
        Ok(())
    } else {
        Err(Error::Storage)
    }
}

#[derive(FromRow)]
#[orm(entity = crate::models::jobs)]
struct Candidate {
    id: String,
}

/// Filter readiness before limiting candidates, so blocked cron work cannot
/// hide a deliverable activation or recovery job later in the app's queue.
pub(crate) async fn candidate(
    tx: &Database,
    app: &AppId,
    now: i64,
) -> Result<Option<String>, Error> {
    use crate::models::jobs;
    use zeroship_core::workflow_jobs::JobOutcome;
    use zeroship_data_orm::sql::Predicate;
    let job = tx.entity::<jobs::Entity>()?.alias("j")?;
    let occurrence = tx.entity::<schedule_occurrences::Entity>()?.alias("o")?;
    let activation = tx.entity::<jobs::Entity>()?.alias("a")?;
    Ok(tx
        .from(&job)
        .left_join(
            &occurrence,
            Predicate::And(vec![
                job.column(jobs::app_id)
                    .eq_column(occurrence.column(schedule_occurrences::app_id))?,
                job.column(jobs::id)
                    .eq_column(occurrence.column(schedule_occurrences::job_id))?,
            ]),
        )?
        .left_join(
            &activation,
            Predicate::And(vec![
                occurrence
                    .column(schedule_occurrences::app_id)
                    .eq_column(activation.column(jobs::app_id))?,
                occurrence
                    .column(schedule_occurrences::activation_id)
                    .eq_column(activation.column(jobs::id))?,
            ]),
        )?
        .filter(Predicate::And(vec![
            job.column(jobs::app_id).eq(app.as_str())?,
            Predicate::Or(vec![
                Predicate::And(vec![
                    job.column(jobs::state).eq("ready")?,
                    job.column(jobs::available_at).lte(now)?,
                ]),
                Predicate::And(vec![
                    job.column(jobs::state).eq("leased")?,
                    job.column(jobs::lease_deadline).lte(Some(now))?,
                ]),
            ]),
            Predicate::Or(vec![
                occurrence.column(schedule_occurrences::id).is_null(),
                Predicate::And(vec![
                    activation.column(jobs::state).eq("settled")?,
                    activation.column(jobs::outcome).eq(Some(
                        serde_json::to_string(&JobOutcome::Completed)
                            .map_err(|_| Error::Storage)?,
                    ))?,
                ]),
            ]),
        ]))
        .order_by(job.column(jobs::available_at).asc())
        .order_by(job.column(jobs::id).asc())
        .select(job.row::<Candidate>())?
        .limit(1)?
        .all()
        .await?
        .into_iter()
        .next()
        .map(|row| row.id))
}
