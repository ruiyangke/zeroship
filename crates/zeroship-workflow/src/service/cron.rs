//! Accept manager-selected occurrences without evaluating a creator calendar.

#![expect(
    clippy::future_not_send,
    reason = "creator acceptance owns compio-local journal and artifact I/O"
)]

use super::{
    activation,
    app::{decode, encode, insert_root_run, live_runs, lock_app_state},
    delivery::{self, CapturedLease, JobReceipt},
    deployment_retention::admission_generation,
    deployments::unavailable,
    deploys,
    models::{activations, job_receipts, occurrences, runs, schedules},
    store::Transaction,
    AppWorkflows, DeployRegistration, ScheduleOverlap, ScheduleRegistration,
};
use crate::service::policy::admit;
use crate::{
    operations::{RunState, StartOptions},
    validation, WorkflowServiceError,
};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{RequestId, Revision, RunId, UnixMillis},
    workflow_jobs::{DeploymentId, JobLease, JobOperation, JobOutcome, JobSpec},
    workflow_schedules::ScheduleId,
};
use zeroship_data_orm::orm::{FindOptions, FromRow, Insertable};

#[derive(FromRow, Insertable)]
#[orm(entity = schedules)]
struct Schedule {
    id: String,
    app_id: String,
    name: String,
}

#[derive(FromRow, Insertable)]
#[orm(entity = occurrences)]
struct Occurrence {
    id: String,
    app_id: String,
    schedule_id: String,
    revision: i64,
    at: i64,
    job_id: String,
    run_id: Option<String>,
}

#[derive(FromRow)]
#[orm(entity = activations)]
struct Readiness {
    id: String,
    deploy_id: String,
}

#[derive(FromRow)]
#[orm(entity = job_receipts)]
struct Specification {
    specification: String,
}

#[derive(FromRow, Insertable)]
#[orm(entity = job_receipts)]
struct NewReceipt {
    id: String,
    app_id: String,
    run_id: Option<String>,
    specification: String,
    created_at: i64,
}

struct Cron<'a> {
    deployment_id: &'a DeploymentId,
    schedule_id: &'a ScheduleId,
    schedule_name: &'a str,
    request_id: &'a RequestId,
    run_id: &'a RunId,
    revision: Revision,
    scheduled_at: UnixMillis,
}

impl<'a> Cron<'a> {
    fn from_job(job: &'a JobSpec) -> Result<Self, WorkflowServiceError> {
        let JobOperation::Cron {
            deployment_id,
            schedule_id,
            schedule_name,
            request_id,
            run_id,
            revision,
            scheduled_at,
        } = &job.operation
        else {
            return Err(WorkflowServiceError::InvalidRequest(
                "expected workflow cron job".into(),
            ));
        };
        validation::workflow_name(schedule_name)?;
        Ok(Self {
            deployment_id,
            schedule_id,
            schedule_name,
            request_id,
            run_id,
            revision: *revision,
            scheduled_at: *scheduled_at,
        })
    }

    fn declaration<'d>(
        &self,
        deploy: &'d DeployRegistration,
    ) -> Result<&'d ScheduleRegistration, WorkflowServiceError> {
        let mut matching = deploy
            .schedules
            .iter()
            .filter(|schedule| schedule.name == self.schedule_name);
        let registration = matching.next().ok_or_else(conflict)?;
        if matching.next().is_some() || !deploy.workflows.contains(&registration.workflow_name) {
            return Err(invalid());
        }
        Ok(registration)
    }

    async fn occurrence(
        &self,
        tx: &Transaction,
        app: &AppId,
    ) -> Result<Option<Occurrence>, WorkflowServiceError> {
        let rows = tx
            .database()
            .entity::<occurrences::Entity>()?
            .find::<Occurrence>(
                occurrences::app_id.eq(app.as_str())?.and(
                    occurrences::id
                        .eq(self.request_id.as_str())?
                        .or(occurrences::schedule_id
                            .eq(self.schedule_id.as_str())?
                            .and(occurrences::revision.eq(self.revision.get())?)
                            .and(occurrences::at.eq(self.scheduled_at.get())?)),
                ),
                FindOptions {
                    limit: Some(2),
                    ..Default::default()
                },
            )
            .await?;
        if rows.len() > 1 {
            return Err(invalid());
        }
        Ok(rows.into_iter().next())
    }

    async fn binding(&self, tx: &Transaction, app: &AppId) -> Result<bool, WorkflowServiceError> {
        let rows = tx
            .database()
            .entity::<schedules::Entity>()?
            .find::<Schedule>(
                schedules::app_id.eq(app.as_str())?.and(
                    schedules::id
                        .eq(self.schedule_id.as_str())?
                        .or(schedules::name.eq(self.schedule_name)?),
                ),
                FindOptions {
                    limit: Some(2),
                    ..Default::default()
                },
            )
            .await?;
        if rows.len() > 1 {
            return Err(conflict());
        }
        if let Some(row) = rows.first() {
            if row.id != self.schedule_id.as_str() || row.name != self.schedule_name {
                return Err(conflict());
            }
            return Ok(true);
        }
        Ok(false)
    }
}

impl AppWorkflows {
    /// Admit an exact manager occurrence and publish its initial Advance intent.
    /// Static input comes from the retained activated deployment. An overlap skip
    /// is durable; unavailable code, capacity and authority failures remain retryable.
    ///
    /// # Errors
    /// Rejects foreign or changed identities, expired authority, missing readiness,
    /// unavailable artifacts and creator admission or storage failures.
    #[expect(
        clippy::too_many_lines,
        reason = "acceptance rechecks captured authority around retention and the atomic creator commit"
    )]
    pub async fn cron_job(
        &self,
        lease: &impl JobLease,
    ) -> Result<JobReceipt, WorkflowServiceError> {
        let job = &lease.delivery().job;
        delivery::check_scope(self.app_id(), job)?;
        let cron = Cron::from_job(job)?;
        let captured = CapturedLease::capture(self, lease);
        let budget = delivery::attempt_budget(captured.as_ref().ok(), None);
        delivery::run_attempt(
            captured.as_ref().ok().map(CapturedLease::cancelled),
            budget,
            Box::pin(async {
                let mut tx = self.service.begin().await?;
                lock_app_state(&mut tx, self.app_id()).await?;
                if let Some(receipt) = receipt(&tx, job).await? {
                    tx.commit().await?;
                    return Ok(receipt);
                }
                let authority = captured?;
                authority.check(self)?;
                require_ready(&tx, job, &cron).await?;
                require_new(&tx, job, &cron).await?;
                let previous = deploys::read(&tx, self.app_id(), cron.deployment_id.as_str())
                    .await?
                    .ok_or_else(unavailable)?;
                let expected = previous.registration()?;
                if expected.id != cron.deployment_id.as_str() || expected.hash != previous.hash {
                    return Err(invalid());
                }
                cron.declaration(&expected)?;
                authority.check(self)?;
                tx.commit().await?;
                authority.check(self)?;

                let source = self.service.deployments.as_ref().ok_or_else(unavailable)?;
                let client = source.client(self.app_id())?;
                let held = self
                    .service
                    .acquire_deployment_hold_checked(
                        self.app_id(),
                        cron.deployment_id.as_str(),
                        Some(&expected.hash),
                        client.as_ref(),
                        &|| authority.check(self),
                    )
                    .await?;
                authority.check(self)?;
                let executable = source.read(self.app_id(), &held.deploy_hash).await?;
                authority.check(self)?;
                let deployment =
                    executable.registration(expected.id.clone(), held.deploy_hash.clone());
                if deployment != expected {
                    return Err(invalid());
                }
                let registration = cron.declaration(&deployment)?;

                let mut tx = self.service.begin().await?;
                lock_app_state(&mut tx, self.app_id()).await?;
                if let Some(receipt) = receipt(&tx, job).await? {
                    tx.commit().await?;
                    return Ok(receipt);
                }
                authority.check(self)?;
                require_ready(&tx, job, &cron).await?;
                require_new(&tx, job, &cron).await?;
                if admission_generation(
                    &tx,
                    self.app_id(),
                    &deployment.id,
                    &deployment.hash,
                    client.scope(),
                )
                .await?
                    != held.generation.get()
                {
                    return Err(conflict());
                }
                let policy = authority.policy();
                admit(policy)?;
                if encode(&registration.input)?.len() > policy.max_input_bytes {
                    return Err(WorkflowServiceError::PayloadTooLarge);
                }
                let skip = registration.overlap == ScheduleOverlap::SkipIfRunning
                    && tx
                        .database()
                        .entity::<runs::Entity>()?
                        .exists(
                            runs::app_id
                                .eq(self.app_id().as_str())?
                                .and(runs::schedule_id.eq(Some(cron.schedule_id.as_str()))?)
                                .and(runs::state.not_in_values(RunState::TERMINAL)?),
                        )
                        .await?;
                if !skip && live_runs(&tx, self.app_id()).await? >= policy.max_live_runs {
                    return Err(WorkflowServiceError::ResourceExhausted(
                        "workflow live-run limit reached".into(),
                    ));
                }
                let now = tx.now().await?;
                deploys::record_verified(&tx, self.app_id(), &deployment, now).await?;
                if !cron.binding(&tx, self.app_id()).await? {
                    tx.database()
                        .entity::<schedules::Entity>()?
                        .insert::<_, Schedule>(Schedule {
                            id: cron.schedule_id.as_str().into(),
                            app_id: self.app_id().as_str().into(),
                            name: cron.schedule_name.into(),
                        })
                        .await?;
                }
                let run_id = (!skip).then(|| cron.run_id.as_str().to_owned());
                if !skip {
                    insert_root_run(
                        &mut tx,
                        self.app_id(),
                        cron.run_id.as_str(),
                        &registration.workflow_name,
                        &deployment.id,
                        &StartOptions {
                            input: registration.input.clone(),
                            ..Default::default()
                        },
                        now,
                    )
                    .await?;
                    let changed = tx
                        .database()
                        .entity::<runs::Entity>()?
                        .update_many(
                            runs::app_id
                                .eq(self.app_id().as_str())?
                                .and(runs::id.eq(cron.run_id.as_str())?),
                            runs::schedule_id.set(Some(cron.schedule_id.as_str()))?,
                        )
                        .await?;
                    if changed != 1 {
                        return Err(invalid());
                    }
                }
                tx.database()
                    .entity::<job_receipts::Entity>()?
                    .insert::<_, NewReceipt>(NewReceipt {
                        id: job.id.as_str().into(),
                        app_id: self.app_id().as_str().into(),
                        run_id: run_id.clone(),
                        specification: encode(job)?,
                        created_at: now,
                    })
                    .await?;
                tx.database()
                    .entity::<occurrences::Entity>()?
                    .insert::<_, Occurrence>(Occurrence {
                        id: cron.request_id.as_str().into(),
                        app_id: self.app_id().as_str().into(),
                        schedule_id: cron.schedule_id.as_str().into(),
                        revision: cron.revision.get(),
                        at: cron.scheduled_at.get(),
                        job_id: job.id.as_str().into(),
                        run_id,
                    })
                    .await?;
                let outcome = if skip {
                    JobOutcome::Rejected {}
                } else {
                    JobOutcome::Completed {}
                };
                let receipt = delivery::finish(&tx, job, outcome, now).await?;
                authority.check(self)?;
                tx.commit().await?;
                Ok(receipt)
            }),
        )
        .await
    }
}

pub(super) async fn receipt(
    tx: &Transaction,
    job: &JobSpec,
) -> Result<Option<JobReceipt>, WorkflowServiceError> {
    let Some(record) = delivery::read(tx, job).await? else {
        return Ok(None);
    };
    let receipt = record.receipt(job)?.ok_or_else(invalid)?;
    let cron = Cron::from_job(job)?;
    let occurrence = cron
        .occurrence(tx, &job.app_id)
        .await?
        .ok_or_else(invalid)?;
    if occurrence.id != cron.request_id.as_str()
        || occurrence.schedule_id != cron.schedule_id.as_str()
        || occurrence.revision != cron.revision.get()
        || occurrence.at != cron.scheduled_at.get()
        || occurrence.job_id != job.id.as_str()
        || !cron.binding(tx, &job.app_id).await?
    {
        return Err(invalid());
    }
    match receipt.outcome {
        JobOutcome::Completed {} if occurrence.run_id.as_deref() == Some(cron.run_id.as_str()) => {}
        JobOutcome::Rejected {} if occurrence.run_id.is_none() => {}
        _ => return Err(invalid()),
    }
    Ok(Some(receipt))
}

async fn require_new(
    tx: &Transaction,
    job: &JobSpec,
    cron: &Cron<'_>,
) -> Result<(), WorkflowServiceError> {
    if cron.occurrence(tx, &job.app_id).await?.is_some() {
        return Err(conflict());
    }
    cron.binding(tx, &job.app_id).await?;
    if tx
        .database()
        .entity::<runs::Entity>()?
        .exists(
            runs::app_id
                .eq(job.app_id.as_str())?
                .and(runs::id.eq(cron.run_id.as_str())?),
        )
        .await?
    {
        return Err(conflict());
    }
    Ok(())
}

async fn require_ready(
    tx: &Transaction,
    job: &JobSpec,
    cron: &Cron<'_>,
) -> Result<(), WorkflowServiceError> {
    let readiness = tx
        .database()
        .entity::<activations::Entity>()?
        .find::<Readiness>(
            activations::app_id
                .eq(job.app_id.as_str())?
                .and(activations::revision.eq(cron.revision.get())?),
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(unavailable)?;
    if readiness.deploy_id != cron.deployment_id.as_str() {
        return Err(conflict());
    }
    let record = tx
        .database()
        .entity::<job_receipts::Entity>()?
        .find::<Specification>(
            job_receipts::app_id
                .eq(job.app_id.as_str())?
                .and(job_receipts::id.eq(readiness.id.as_str())?),
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(invalid)?;
    let activation: JobSpec = decode(&record.specification)?;
    if activation.app_id != job.app_id
        || activation.id.as_str() != readiness.id
        || activation.operation
            != (JobOperation::Activate {
                deployment_id: cron.deployment_id.clone(),
                revision: cron.revision,
            })
    {
        return Err(invalid());
    }
    activation::receipt(tx, &activation)
        .await?
        .ok_or_else(invalid)?;
    Ok(())
}

fn conflict() -> WorkflowServiceError {
    WorkflowServiceError::Conflict("workflow occurrence identity changed".into())
}
fn invalid() -> WorkflowServiceError {
    WorkflowServiceError::Internal("invalid workflow occurrence journal".into())
}
