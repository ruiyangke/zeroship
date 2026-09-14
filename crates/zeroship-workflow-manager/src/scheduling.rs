//! Manager-owned calendar frontiers and activation-gated occurrence delivery.
#![expect(
    clippy::future_not_send,
    reason = "scheduling uses the manager's compio-local ORM"
)]

use crate::{
    models::schema::{
        schedule_activations, schedule_deployments, schedule_disables, schedule_occurrences,
        schedule_scopes, schedules,
    },
    queue::{self, Budget},
    recovery,
    retention::{self, Retention},
    Error, Queue,
};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{RequestId, Revision, RunId},
    workflow_jobs::{DeploymentId, JobId, JobOperation, JobSpec},
    workflow_schedules::{
        ActivateSchedules, DisableSchedules, RegisterSchedules, ScheduleDescriptor, ScheduleId,
    },
};
use zeroship_data_orm::orm::{Database, FindOptions, FromRow, Insertable};
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

impl Options {
    /// Refuse schedule metadata that preparation or activation would refuse
    /// under these bounds. A deployment host checks its projection before
    /// accepting a deployment, so a committed publication cannot be refused
    /// for its content. Schedule order is irrelevant.
    ///
    /// # Errors
    /// `Capacity` for too many schedules or an excessive backfill allowance,
    /// `Invalid` for reserved or malformed names, intervals and calendars, and
    /// `Conflict` for a repeated schedule name.
    pub fn validate(&self, schedules: &[ScheduleDescriptor]) -> Result<(), Error> {
        if schedules.len() > self.max_schedules {
            return Err(Error::Capacity);
        }
        let mut names = std::collections::BTreeSet::new();
        for descriptor in schedules {
            for name in [&descriptor.name, &descriptor.workflow_name] {
                if name.is_empty() || name.len() > 128 || name.starts_with("__zs.") {
                    return Err(Error::Invalid);
                }
            }
            if let ScheduleCatchUp::Backfill { max } = descriptor.catch_up {
                if max == 0 || max > self.max_backfill {
                    return Err(Error::Capacity);
                }
            }
            if let ScheduleTiming::Interval { interval_ms, .. } = descriptor.schedule {
                if interval_ms < self.min_interval_ms {
                    return Err(Error::Invalid);
                }
            }
            descriptor
                .schedule
                .next_after(0, 0)
                .map_err(|_| Error::Invalid)?;
            if !names.insert(descriptor.name.as_str()) {
                return Err(Error::Conflict);
            }
        }
        Ok(())
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

/// An app's current calendar lifecycle state. The platform chooses the next
/// activation revision above `revision`; retries of an accepted activation
/// reuse their original revision instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// The highest activation or disable revision accepted for the app.
    pub revision: Revision,
    pub enabled: bool,
    /// The most recently selected activation, retained while disabled.
    pub activation: Option<SelectedActivation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedActivation {
    /// The stable activation job that delivers creator readiness.
    pub job: JobSpec,
    pub deployment_id: DeploymentId,
    pub revision: Revision,
    /// The activation job settled as completed, so its occurrences may dispatch.
    pub ready: bool,
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
    enabled: bool,
    activation_id: Option<String>,
}

#[derive(FromRow)]
#[orm(entity = schedule_disables)]
struct Disabled {
    id: String,
    created_at: i64,
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
pub(crate) struct Due {
    pub(crate) id: String,
    pub(crate) app_id: String,
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

#[derive(Insertable)]
#[orm(entity = schedule_deployments)]
struct NewPrepared<'a> {
    id: &'a str,
    app_id: &'a str,
    definition: &'a str,
    interpretation: String,
    created_at: i64,
}

#[derive(Insertable)]
#[orm(entity = schedule_activations)]
struct NewActivation<'a> {
    id: &'a str,
    app_id: &'a str,
    deployment_id: &'a str,
    revision: i64,
    activated_at: i64,
}

#[derive(Insertable)]
#[orm(entity = schedule_scopes)]
struct NewScope<'a> {
    id: &'a str,
    revision: i64,
    enabled: bool,
    activation_id: Option<&'a str>,
}

#[derive(Insertable)]
#[orm(entity = schedule_disables)]
struct NewDisable<'a> {
    id: String,
    app_id: &'a str,
    revision: i64,
    created_at: i64,
}

#[derive(Insertable)]
#[orm(entity = schedules)]
struct NewSchedule<'a> {
    id: String,
    app_id: &'a str,
    name: &'a str,
    activation_id: &'a str,
    revision: i64,
    definition: &'a str,
    next_at: Option<i64>,
    anchor_at: i64,
    catch_up_until: Option<i64>,
    catch_up_remaining: Option<i64>,
}

#[derive(Insertable)]
#[orm(entity = schedule_occurrences)]
struct NewOccurrence<'a> {
    id: &'a str,
    app_id: &'a str,
    schedule_id: &'a str,
    revision: i64,
    scheduled_at: i64,
    run_id: &'a str,
    job_id: &'a str,
    activation_id: &'a str,
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
        self.queue
            .transact(|tx| async move {
                queue::register_scope_in(&tx, &request.app_id).await?;
                queue::lock_scope(&tx, &request.app_id).await?;
                if let Some(existing) =
                    prepared(&tx, &request.app_id, &request.deployment_id).await?
                {
                    return if existing.definition == definition {
                        Ok(())
                    } else {
                        Err(Error::Conflict)
                    };
                }
                tx.entity::<schedule_deployments::Entity>()?
                    .insert::<_, Prepared>(NewPrepared {
                        id: request.deployment_id.as_str(),
                        app_id: request.app_id.as_str(),
                        definition: &definition,
                        interpretation: zeroship_workflow_calendar::interpretation(),
                        created_at: self.queue.clock.now().await?,
                    })
                    .await?;
                Ok(())
            })
            .await
    }

    fn validate(&self, request: &RegisterSchedules) -> Result<(), Error> {
        self.options.validate(&request.schedules)
    }

    /// Stop calendar production while preserving accepted jobs and recovery.
    ///
    /// # Errors
    /// Rejects revisions reused by activation, unknown stale commands and storage
    /// failures. Exact accepted commands replay without changing newer state.
    pub async fn disable(&self, request: &DisableSchedules) -> Result<DisableSchedules, Error> {
        self.queue
            .transact(|tx| async move {
                queue::register_scope_in(&tx, &request.app_id).await?;
                queue::lock_scope(&tx, &request.app_id).await?;
                if activation_at_revision(&tx, &request.app_id, request.revision)
                    .await?
                    .is_some()
                {
                    return Err(Error::Conflict);
                }
                if disabled(&tx, &request.app_id, request.revision.get())
                    .await?
                    .is_some()
                {
                    return Ok(request.clone());
                }
                let previous = active(&tx, &request.app_id).await?;
                if previous
                    .as_ref()
                    .is_some_and(|scope| scope.revision >= request.revision.get())
                {
                    return Err(Error::Conflict);
                }
                tx.entity::<schedule_disables::Entity>()?
                    .insert::<_, Disabled>(NewDisable {
                        id: RequestId::mint().as_str().into(),
                        app_id: request.app_id.as_str(),
                        revision: request.revision.get(),
                        created_at: self.queue.clock.now().await?,
                    })
                    .await?;
                if let Some(previous) = previous {
                    changed(
                        tx.entity::<schedule_scopes::Entity>()?
                            .update_many(
                                schedule_scopes::id
                                    .eq(request.app_id.as_str())?
                                    .and(schedule_scopes::revision.eq(previous.revision)?),
                                schedule_scopes::revision
                                    .set(request.revision.get())?
                                    .and(schedule_scopes::enabled.set(false)?)?,
                            )
                            .await?,
                    )?;
                } else {
                    tx.entity::<schedule_scopes::Entity>()?
                        .insert::<_, Active>(NewScope {
                            id: request.app_id.as_str(),
                            revision: request.revision.get(),
                            enabled: false,
                            activation_id: None,
                        })
                        .await?;
                }
                Ok(request.clone())
            })
            .await
    }

    /// Select a deployment, resuming retained calendars when restoring the same
    /// disabled deployment. Readiness still belongs to this activation's job.
    ///
    /// # Errors
    /// Rejects unprepared deployments, stale/conflicting revisions, changed calendar
    /// interpretation and failed storage. Preparation alone grants no activation.
    #[expect(
        clippy::too_many_lines,
        reason = "activation retains its lock, publication and schedule replacement in one transaction"
    )]
    pub async fn activate(&self, request: &ActivateSchedules) -> Result<JobSpec, Error> {
        let budget = Budget::new(self.queue.options.transaction_timeout);
        loop {
            let result = self
                .queue
                .transact_for(budget.clone(), |tx| async move {
                    queue::lock_scope(&tx, &request.app_id).await?;
                    if disabled(&tx, &request.app_id, request.revision.get())
                        .await?
                        .is_some()
                    {
                        return Err(Error::Conflict);
                    }
                    let existing =
                        activation_at_revision(&tx, &request.app_id, request.revision).await?;
                    if let Some(existing) = existing {
                        if existing.deployment_id != request.deployment_id.as_str() {
                            return Err(Error::Conflict);
                        }
                        return Ok(Retention::Ready(
                            activation_job(&tx, &request.app_id, &existing)
                                .await?
                                .spec()?,
                        ));
                    }
                    let active = active(&tx, &request.app_id).await?;
                    if active
                        .as_ref()
                        .is_some_and(|active| active.revision >= request.revision.get())
                    {
                        return Err(Error::Conflict);
                    }
                    let prepared = prepared(&tx, &request.app_id, &request.deployment_id)
                        .await?
                        .ok_or(Error::Denied)?;
                    check_interpretation(&prepared)?;
                    let metadata = metadata(&prepared, &request.app_id, &request.deployment_id)?;
                    self.validate(&metadata)?;
                    let restore =
                        if let Some(previous) = active.as_ref().filter(|scope| !scope.enabled) {
                            if let Some(id) = &previous.activation_id {
                                let previous = load_activation(&tx, &request.app_id, id).await?;
                                (previous.deployment_id == request.deployment_id.as_str())
                                    .then_some(previous)
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                    if !retention::prepared(&tx, &request.app_id, &request.deployment_id).await? {
                        return Ok(Retention::Acquire(request.deployment_id.clone()));
                    }
                    let now = self.queue.clock.now().await?;
                    let job = JobSpec {
                        id: JobId::mint(),
                        app_id: request.app_id.clone(),
                        operation: JobOperation::Activate {
                            deployment_id: request.deployment_id.clone(),
                            revision: request.revision,
                        },
                        available_at: now.try_into().map_err(|_| Error::Storage)?,
                    };
                    self.queue.insert(&tx, &job, now).await?;
                    tx.entity::<schedule_activations::Entity>()?
                        .insert::<_, Activation>(NewActivation {
                            id: job.id.as_str(),
                            app_id: request.app_id.as_str(),
                            deployment_id: request.deployment_id.as_str(),
                            revision: request.revision.get(),
                            activated_at: now,
                        })
                        .await?;
                    if let Some(active) = active {
                        changed(
                            tx.entity::<schedule_scopes::Entity>()?
                                .update_many(
                                    schedule_scopes::id
                                        .eq(request.app_id.as_str())?
                                        .and(schedule_scopes::revision.eq(active.revision)?)
                                        .and(
                                            schedule_scopes::activation_id
                                                .eq(active.activation_id.as_deref())?,
                                        ),
                                    schedule_scopes::revision
                                        .set(request.revision.get())?
                                        .and(
                                            schedule_scopes::activation_id
                                                .set(Some(job.id.as_str()))?,
                                        )?
                                        .and(schedule_scopes::enabled.set(true)?)?,
                                )
                                .await?,
                        )?;
                    } else {
                        tx.entity::<schedule_scopes::Entity>()?
                            .insert::<_, Active>(NewScope {
                                id: request.app_id.as_str(),
                                revision: request.revision.get(),
                                enabled: true,
                                activation_id: Some(job.id.as_str()),
                            })
                            .await?;
                    }
                    if let Some(previous) = restore {
                        Box::pin(Self::restore(&tx, request, &job.id, &previous, &metadata))
                            .await?;
                    } else {
                        tx.entity::<schedules::Entity>()?
                            .update_many(
                                schedules::app_id.eq(request.app_id.as_str())?,
                                schedules::next_at
                                    .set(None::<i64>)?
                                    .and(schedules::catch_up_until.set(None::<i64>)?)?
                                    .and(schedules::catch_up_remaining.set(None::<i64>)?)?,
                            )
                            .await?;
                        for descriptor in &metadata.schedules {
                            Box::pin(self.install(&tx, request, &job.id, descriptor, now)).await?;
                        }
                    }
                    recovery::ensure_in(
                        &tx,
                        &request.app_id,
                        &request.deployment_id,
                        request.revision,
                        now,
                    )
                    .await?;
                    Ok(Retention::Ready(job))
                })
                .await?;
            match result {
                Retention::Ready(job) => return Ok(job),
                Retention::Acquire(deployment) => {
                    self.queue
                        .ensure_deployment_for(&request.app_id, &deployment, budget.clone())
                        .await?;
                }
            }
        }
    }

    /// Read the app's current calendar selection and its activation readiness
    /// under the app lock. An app without scheduling state has no selection.
    ///
    /// # Errors
    /// Reports malformed stored selections and unavailable manager storage.
    pub async fn selection(&self, app: &AppId) -> Result<Option<Selection>, Error> {
        use zeroship_core::workflow_jobs::JobOutcome;
        self.queue
            .transact(|tx| async move {
                match queue::lock_scope(&tx, app).await {
                    Ok(()) => {}
                    Err(Error::Denied) => return Ok(None),
                    Err(error) => return Err(error),
                }
                let Some(scope) = active(&tx, app).await? else {
                    return Ok(None);
                };
                let activation = if let Some(id) = &scope.activation_id {
                    let activation = load_activation(&tx, app, id).await?;
                    let job = activation_job(&tx, app, &activation).await?;
                    let completed = serde_json::to_string(&JobOutcome::Completed {})
                        .map_err(|_| Error::Storage)?;
                    Some(SelectedActivation {
                        deployment_id: DeploymentId::parse(&activation.deployment_id)
                            .map_err(|_| Error::Storage)?,
                        revision: activation.revision.try_into().map_err(|_| Error::Storage)?,
                        ready: job.state == "settled" && job.outcome.as_deref() == Some(&completed),
                        job: job.spec()?,
                    })
                } else {
                    None
                };
                Ok(Some(Selection {
                    revision: scope.revision.try_into().map_err(|_| Error::Storage)?,
                    enabled: scope.enabled,
                    activation,
                }))
            })
            .await
    }

    async fn restore(
        tx: &Database,
        request: &ActivateSchedules,
        job: &JobId,
        previous: &Activation,
        metadata: &RegisterSchedules,
    ) -> Result<(), Error> {
        let limit = i64::try_from(metadata.schedules.len())
            .map_err(|_| Error::Storage)?
            .checked_add(1)
            .ok_or(Error::Storage)?;
        let saved = tx
            .entity::<schedules::Entity>()?
            .query()
            .filter(
                schedules::app_id
                    .eq(request.app_id.as_str())?
                    .and(schedules::activation_id.eq(previous.id.as_str())?)
                    .and(
                        schedules::next_at
                            .ne(None::<i64>)?
                            .or(schedules::catch_up_until.ne(None::<i64>)?)
                            .or(schedules::catch_up_remaining.ne(None::<i64>)?),
                    ),
            )
            .limit(limit)?
            .all::<Schedule>()
            .await?;
        if saved.len() != metadata.schedules.len() {
            return Err(Error::Storage);
        }
        for schedule in saved {
            let definition = descriptor(metadata, &schedule.name)?;
            if schedule.activation_id != previous.id {
                return Err(Error::Storage);
            }
            if schedule.revision != previous.revision
                || schedule.next_at.is_none()
                || serde_json::from_str::<ScheduleDescriptor>(&schedule.definition)
                    .map_err(|_| Error::Storage)?
                    != *definition
            {
                return Err(Error::Storage);
            }
            let allowance = match definition.catch_up {
                ScheduleCatchUp::Skip => 1,
                ScheduleCatchUp::Backfill { max } => {
                    i64::try_from(max).map_err(|_| Error::Storage)?
                }
            };
            match (schedule.catch_up_until, schedule.catch_up_remaining) {
                (None, None) => {}
                (Some(boundary), Some(remaining))
                    if remaining > 0
                        && remaining <= allowance
                        && schedule.next_at.is_some_and(|at| at <= boundary) => {}
                _ => return Err(Error::Storage),
            }
            changed(
                tx.entity::<schedules::Entity>()?
                    .update_many(
                        schedules::app_id
                            .eq(request.app_id.as_str())?
                            .and(schedules::id.eq(schedule.id.as_str())?)
                            .and(schedules::activation_id.eq(previous.id.as_str())?)
                            .and(schedules::revision.eq(previous.revision)?),
                        schedules::activation_id
                            .set(job.as_str())?
                            .and(schedules::revision.set(request.revision.get())?)?,
                    )
                    .await?,
            )?;
        }
        Ok(())
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
        let collection = tx.entity::<schedules::Entity>()?;
        if let Some(previous) = previous {
            changed(
                collection
                    .update_many(
                        schedules::id
                            .eq(previous.id.as_str())?
                            .and(schedules::app_id.eq(activation.app_id.as_str())?),
                        schedules::activation_id
                            .set(job.as_str())?
                            .and(schedules::revision.set(activation.revision.get())?)?
                            .and(schedules::definition.set(definition.as_str())?)?
                            .and(schedules::next_at.set(Some(next))?)?
                            .and(schedules::anchor_at.set(now)?)?
                            .and(schedules::catch_up_until.set(None::<i64>)?)?
                            .and(schedules::catch_up_remaining.set(None::<i64>)?)?,
                    )
                    .await?,
            )?;
        } else {
            collection
                .insert::<_, Schedule>(NewSchedule {
                    id: ScheduleId::mint().as_str().into(),
                    app_id: activation.app_id.as_str(),
                    name: &descriptor.name,
                    activation_id: job.as_str(),
                    revision: activation.revision.get(),
                    definition: &definition,
                    next_at: Some(next),
                    anchor_at: now,
                    catch_up_until: None,
                    catch_up_remaining: None,
                })
                .await?;
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
                due_in(
                    &tx,
                    self.queue.clock.now().await?,
                    after.map(ScheduleId::as_str),
                    None,
                    false,
                    self.options.page_size,
                )
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
    #[expect(
        clippy::too_many_lines,
        reason = "calendar occurrence publication and cursor advancement share a fenced transaction"
    )]
    pub async fn dispatch(&self, app: &AppId, id: &ScheduleId) -> Result<Page, Error> {
        self.queue
            .transact(|tx| async move {
                queue::lock_scope(&tx, app).await?;
                let Some(record) = selected_schedule(&tx, app, id).await? else {
                    return Ok(Page {
                        jobs: vec![],
                        more: false,
                    });
                };
                let now = self.queue.clock.now().await?;
                let Some(mut at) = record.next_at.filter(|at| *at <= now) else {
                    return Ok(Page {
                        jobs: vec![],
                        more: false,
                    });
                };
                let activation = load_activation(&tx, app, &record.activation_id).await?;
                if activation.revision != record.revision {
                    return Err(Error::Storage);
                }
                activation_job(&tx, app, &activation).await?;
                let deployment =
                    DeploymentId::parse(&activation.deployment_id).map_err(|_| Error::Storage)?;
                let prepared = prepared(&tx, app, &deployment)
                    .await?
                    .ok_or(Error::Storage)?;
                check_interpretation(&prepared)?;
                let metadata = metadata(&prepared, app, &deployment)?;
                let definition: ScheduleDescriptor =
                    serde_json::from_str(&record.definition).map_err(|_| Error::Storage)?;
                if definition.name != record.name
                    || descriptor(&metadata, &record.name)? != &definition
                {
                    return Err(Error::Storage);
                }
                let allowance = match definition.catch_up {
                    ScheduleCatchUp::Skip => 1,
                    ScheduleCatchUp::Backfill { max } => {
                        i64::try_from(max).map_err(|_| Error::Storage)?
                    }
                };
                let (boundary, mut remaining) =
                    match (record.catch_up_until, record.catch_up_remaining) {
                        (Some(boundary), Some(remaining))
                            if boundary >= at && remaining > 0 && remaining <= allowance =>
                        {
                            (boundary, remaining)
                        }
                        (None, None) if allowance > 0 => (
                            now,
                            allowance.min(
                                i64::try_from(self.options.max_backfill)
                                    .map_err(|_| Error::Invalid)?,
                            ),
                        ),
                        _ => return Err(Error::Storage),
                    };
                let mut jobs = Vec::new();
                while at <= boundary
                    && remaining > 0
                    && jobs.len() < self.options.page_size as usize
                {
                    let job = Box::pin(self.occurrence(
                        &tx,
                        app,
                        id,
                        &record,
                        &definition,
                        &deployment,
                        at,
                        now,
                    ))
                    .await?;
                    jobs.push(job);
                    at = definition
                        .schedule
                        .next_after(at, record.anchor_at)
                        .map_err(|_| Error::Storage)?;
                    remaining -= 1;
                }
                if remaining == 0 && at <= boundary {
                    at = definition
                        .schedule
                        .next_after(boundary, record.anchor_at)
                        .map_err(|_| Error::Storage)?;
                }
                let more = at <= boundary;
                changed(
                    tx.entity::<schedules::Entity>()?
                        .update_many(
                            schedules::id
                                .eq(id.as_str())?
                                .and(schedules::app_id.eq(app.as_str())?)
                                .and(schedules::activation_id.eq(record.activation_id.as_str())?)
                                .and(schedules::revision.eq(record.revision)?),
                            schedules::next_at
                                .set(Some(at))?
                                .and(schedules::catch_up_until.set(more.then_some(boundary))?)?
                                .and(
                                    schedules::catch_up_remaining.set(more.then_some(remaining))?,
                                )?,
                        )
                        .await?,
                )?;
                Ok(Page { jobs, more })
            })
            .await
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
            if !self.queue.existing(tx, &job).await? {
                return Err(Error::Storage);
            }
            return Ok(job);
        }
        let request = RequestId::mint();
        let run = RunId::mint();
        let job = JobSpec {
            id: JobId::mint(),
            app_id: app.clone(),
            operation: JobOperation::Cron {
                deployment_id: deployment.clone(),
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
        tx.entity::<schedule_occurrences::Entity>()?
            .insert::<_, Occurrence>(NewOccurrence {
                id: request.as_str(),
                app_id: app.as_str(),
                schedule_id: id.as_str(),
                revision: record.revision,
                scheduled_at: at,
                run_id: run.as_str(),
                job_id: job.id.as_str(),
                activation_id: &record.activation_id,
            })
            .await?;
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
        operation: JobOperation::Activate {
            deployment_id: DeploymentId::parse(&activation.deployment_id)
                .map_err(|_| Error::Storage)?,
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
    match job.state.as_str() {
        "ready" | "leased" => {
            retention::require_held(tx, app, expected.deployment_id().ok_or(Error::Storage)?)
                .await?;
        }
        "settled" => {}
        _ => return Err(Error::Storage),
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
        deployment_id: deployment.clone(),
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
                        &serde_json::to_string(&JobOutcome::Completed {})
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
    let scope = tx
        .entity::<schedule_scopes::Entity>()?
        .find::<Active>(schedule_scopes::id.eq(app.as_str())?, one())
        .await?
        .into_iter()
        .next();
    if let Some(scope) = &scope {
        Revision::try_from(scope.revision).map_err(|_| Error::Storage)?;
        let disabled = disabled(tx, app, scope.revision).await?.is_some();
        if scope.enabled == disabled {
            return Err(Error::Storage);
        }
        if let Some(id) = &scope.activation_id {
            let activation = load_activation(tx, app, id).await?;
            if (scope.enabled && activation.revision != scope.revision)
                || (!scope.enabled && activation.revision >= scope.revision)
            {
                return Err(Error::Storage);
            }
        } else if scope.enabled {
            return Err(Error::Storage);
        }
    }
    Ok(scope)
}

async fn disabled(tx: &Database, app: &AppId, revision: i64) -> Result<Option<Disabled>, Error> {
    let stored = tx
        .entity::<schedule_disables::Entity>()?
        .query()
        .filter(
            schedule_disables::app_id
                .eq(app.as_str())?
                .and(schedule_disables::revision.eq(revision)?),
        )
        .first::<Disabled>()
        .await?;
    if let Some(stored) = &stored {
        RequestId::parse(&stored.id).map_err(|_| Error::Storage)?;
        if stored.created_at < 0 {
            return Err(Error::Storage);
        }
    }
    Ok(stored)
}

async fn activation_at_revision(
    tx: &Database,
    app: &AppId,
    revision: Revision,
) -> Result<Option<Activation>, Error> {
    Ok(tx
        .entity::<schedule_activations::Entity>()?
        .query()
        .filter(
            schedule_activations::app_id
                .eq(app.as_str())?
                .and(schedule_activations::revision.eq(revision.get())?),
        )
        .first::<Activation>()
        .await?)
}

/// Shared calendar eligibility for the public due page and the driver's bounded
/// identity sweep. Filtering precedes the limit so stopped apps cannot hide work.
pub(crate) async fn due_in(
    tx: &Database,
    now: i64,
    after: Option<&str>,
    upper: Option<&str>,
    descending: bool,
    limit: u32,
) -> Result<Vec<Due>, Error> {
    let schedule = tx.entity::<schedules::Entity>()?.alias("schedule")?;
    let scope = tx.entity::<schedule_scopes::Entity>()?.alias("scope")?;
    let mut filter = scope
        .column(schedule_scopes::enabled)
        .eq(true)?
        .and(schedule.column(schedules::next_at).lte(Some(now))?);
    if let Some(after) = after {
        filter = filter.and(schedule.column(schedules::id).gt(after)?);
    }
    if let Some(upper) = upper {
        filter = filter.and(schedule.column(schedules::id).lte(upper)?);
    }
    Ok(tx
        .from(&schedule)
        .inner_join(
            &scope,
            schedule
                .column(schedules::app_id)
                .eq(scope.column(schedule_scopes::id))?,
        )?
        .filter(filter)
        .order_by(if descending {
            schedule.column(schedules::id).desc()
        } else {
            schedule.column(schedules::id).asc()
        })
        .select(schedule.row::<Due>())?
        .limit(i64::from(limit))?
        .all()
        .await?)
}

// The caller holds the app lock. Disabled identities remain available to their
// accepted occurrences but cannot generate another calendar page.
async fn selected_schedule(
    tx: &Database,
    app: &AppId,
    id: &ScheduleId,
) -> Result<Option<Schedule>, Error> {
    let active = active(tx, app).await?.ok_or(Error::Denied)?;
    if !active.enabled {
        return Ok(None);
    }
    let record = schedule(tx, app, id).await?.ok_or(Error::Denied)?;
    if active.activation_id.as_deref() != Some(record.activation_id.as_str())
        || active.revision != record.revision
    {
        if record.next_at.is_none()
            && record.catch_up_until.is_none()
            && record.catch_up_remaining.is_none()
        {
            return Ok(None);
        }
        return Err(Error::Storage);
    }
    Ok(Some(record))
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
const fn changed(count: i64) -> Result<(), Error> {
    if count == 1 {
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
    let job = tx.entity::<jobs::Entity>()?.alias("j")?;
    let occurrence = tx.entity::<schedule_occurrences::Entity>()?.alias("o")?;
    let activation = tx.entity::<jobs::Entity>()?.alias("a")?;
    let query = tx
        .from(&job)
        .left_join(
            &occurrence,
            job.column(jobs::app_id)
                .eq(occurrence.column(schedule_occurrences::app_id))?
                .and(
                    job.column(jobs::id)
                        .eq(occurrence.column(schedule_occurrences::job_id))?,
                ),
        )?
        .left_join(
            &activation,
            occurrence
                .column(schedule_occurrences::app_id)
                .eq(activation.column(jobs::app_id))?
                .and(
                    occurrence
                        .column(schedule_occurrences::activation_id)
                        .eq(activation.column(jobs::id))?,
                ),
        )?
        .filter(
            job.column(jobs::app_id)
                .eq(app.as_str())?
                .and(
                    job.column(jobs::state)
                        .eq("ready")?
                        .and(job.column(jobs::available_at).lte(now)?)
                        .or(job
                            .column(jobs::state)
                            .eq("leased")?
                            .and(job.column(jobs::lease_deadline).lte(Some(now))?)),
                )
                .and(
                    occurrence
                        .column(schedule_occurrences::id)
                        .is_null()
                        .or(activation.column(jobs::state).eq("settled")?.and(
                            activation.column(jobs::outcome).eq(Some(
                                serde_json::to_string(&JobOutcome::Completed {})
                                    .map_err(|_| Error::Storage)?,
                            ))?,
                        )),
                ),
        );
    Ok(management_eligibility(tx, &job, query)?
        .order_by(job.column(jobs::dispatch_order).asc())
        .order_by(job.column(jobs::id).asc())
        .select(job.row::<Candidate>())?
        .limit(1)?
        .all()
        .await?
        .into_iter()
        .next()
        .map(|row| row.id))
}

fn management_eligibility(
    tx: &Database,
    job: &zeroship_data_orm::orm::EntityAlias<crate::models::jobs::Entity>,
    query: zeroship_data_orm::orm::ReadBuilder,
) -> Result<zeroship_data_orm::orm::ReadBuilder, Error> {
    use crate::models::{jobs, management};
    let command = tx.entity::<management::Entity>()?.alias("command")?;
    let earlier = tx
        .entity::<management::Entity>()?
        .alias("earlier_command")?;
    let barrier = tx.entity::<management::Entity>()?.alias("barrier")?;
    Ok(query
        .left_join(
            &command,
            job.column(jobs::app_id)
                .eq(command.column(management::app_id))?
                .and(job.column(jobs::id).eq(command.column(management::id))?),
        )?
        .left_join(
            &earlier,
            command
                .column(management::app_id)
                .eq(earlier.column(management::app_id))?
                .and(
                    command
                        .column(management::run_id)
                        .eq(earlier.column(management::run_id))?,
                )
                .and(
                    earlier
                        .column(management::revision)
                        .lt(command.column(management::revision))?,
                )
                .and(earlier.column(management::outcome).is_null()),
        )?
        .left_join(
            &barrier,
            job.column(jobs::app_id)
                .eq(barrier.column(management::app_id))?
                .and(
                    job.column(jobs::run_id)
                        .eq(barrier.column(management::run_id))?,
                )
                .and(barrier.column(management::blocks_execution).eq(true)?)
                .and(barrier.column(management::outcome).is_null()),
        )?
        .filter(
            job.column(jobs::operation_kind)
                .eq("management")?
                .negate()
                .or(command
                    .column(management::id)
                    .is_null()
                    .negate()
                    .and(command.column(management::outcome).is_null())
                    .and(earlier.column(management::id).is_null()))
                .and(
                    job.column(jobs::operation_kind)
                        .eq("advance")?
                        .negate()
                        .or(barrier.column(management::id).is_null()),
                ),
        ))
}
