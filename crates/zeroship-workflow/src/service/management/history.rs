use super::{
    conflict, delivery, invalid, Command, JobReceipt, JobSpec, Transaction, WorkflowServiceError,
};
use crate::service::{
    app::{decode, encode},
    models::{job_receipts, management_receipts as receipts, management_scopes as scopes},
    types::{digest, storage_id},
};
use zeroship_core::{
    app_id::AppId, typed_id, workflow_coordination::ManagementOutcome, workflow_jobs::JobOutcome,
};
use zeroship_data_orm::orm::{FindOptions, FromRow, Insertable};

#[derive(FromRow, Insertable)]
#[orm(entity = receipts)]
struct History {
    id: String,
    app_id: String,
    run_id: String,
    request_id: String,
    revision: i64,
    digest: String,
    outcome: String,
    created_at: i64,
}

#[derive(FromRow, Insertable)]
#[orm(entity = scopes)]
pub(super) struct Scope {
    id: String,
    app_id: String,
    run_id: String,
    revision: i64,
}

#[derive(FromRow)]
#[orm(entity = job_receipts)]
struct Specification {
    specification: String,
}

#[derive(Insertable)]
#[orm(entity = job_receipts)]
struct PendingReceipt {
    id: String,
    app_id: String,
    specification: String,
    created_at: i64,
}

pub(super) enum Observed {
    Replay(JobReceipt),
    Fresh(Option<Scope>),
}

impl Observed {
    pub(super) fn require_next(self, revision: i64) -> Result<Option<Scope>, WorkflowServiceError> {
        let Self::Fresh(previous) = self else {
            return Err(invalid());
        };
        let expected = previous
            .as_ref()
            .map_or(0, |scope| scope.revision)
            .checked_add(1)
            .ok_or_else(|| {
                WorkflowServiceError::ResourceExhausted(
                    "workflow management revision exhausted".into(),
                )
            })?;
        if revision != expected {
            return Err(conflict());
        }
        Ok(previous)
    }
}

pub(super) async fn inspect(
    tx: &Transaction,
    command: Command<'_>,
) -> Result<Observed, WorkflowServiceError> {
    let recorded = delivery::read(tx, command.job)
        .await?
        .map(|record| record.receipt(command.job)?.ok_or_else(invalid))
        .transpose()?;
    let anchored = tx
        .database()
        .entity::<receipts::Entity>()?
        .find::<History>(
            receipts::app_id.eq(command.job.app_id.as_str())?.and(
                receipts::id
                    .eq(command.job.id.as_str())?
                    .or(receipts::request_id.eq(command.request_id.as_str())?)
                    .or(receipts::run_id
                        .eq(command.run_id.as_str())?
                        .and(receipts::revision.eq(command.revision)?)),
            ),
            FindOptions {
                limit: Some(4),
                ..Default::default()
            },
        )
        .await?;
    let current = scope(tx, &command.job.app_id, command.run_id.as_str()).await?;
    if let Some(receipt) = recorded {
        if anchored.len() != 1 {
            return Err(invalid());
        }
        let history = &anchored[0];
        check_history(history, command, &receipt)?;
        if current
            .as_ref()
            .is_none_or(|scope| scope.revision < command.revision)
        {
            return Err(invalid());
        }
        return Ok(Observed::Replay(receipt));
    }
    // Independent anchors prevent a damaged request projection from hiding a
    // retained receipt or allowing a new revision to replace its command.
    for history in &anchored {
        validate_saved(tx, history).await?;
    }
    if !anchored.is_empty() {
        return Err(conflict());
    }
    Ok(Observed::Fresh(current))
}

async fn scope(
    tx: &Transaction,
    app: &AppId,
    run: &str,
) -> Result<Option<Scope>, WorkflowServiceError> {
    let current = tx
        .database()
        .entity::<scopes::Entity>()?
        .find::<Scope>(
            scopes::app_id
                .eq(app.as_str())?
                .and(scopes::run_id.eq(run)?),
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next();
    if let Some(current) = &current {
        if current.revision <= 0 || typed_id::parse_with_prefix(&current.id, "wjr").is_err() {
            return Err(invalid());
        }
        let head = tx
            .database()
            .entity::<receipts::Entity>()?
            .find::<History>(
                receipts::app_id
                    .eq(app.as_str())?
                    .and(receipts::run_id.eq(run)?)
                    .and(receipts::revision.eq(current.revision)?),
                FindOptions {
                    limit: Some(1),
                    ..Default::default()
                },
            )
            .await?
            .into_iter()
            .next()
            .ok_or_else(invalid)?;
        validate_saved(tx, &head).await?;
    }
    Ok(current)
}

async fn validate_saved(tx: &Transaction, history: &History) -> Result<(), WorkflowServiceError> {
    let saved = tx
        .database()
        .entity::<job_receipts::Entity>()?
        .find::<Specification>(
            job_receipts::app_id
                .eq(history.app_id.as_str())?
                .and(job_receipts::id.eq(history.id.as_str())?),
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(invalid)?;
    let job: JobSpec = decode(&saved.specification)?;
    let command = Command::read(&job).map_err(|_| invalid())?;
    if job.id.as_str() != history.id || job.app_id.as_str() != history.app_id {
        return Err(invalid());
    }
    let receipt = delivery::read(tx, &job)
        .await?
        .ok_or_else(invalid)?
        .receipt(&job)?
        .ok_or_else(invalid)?;
    check_history(history, command, &receipt)
}

fn check_history(
    history: &History,
    command: Command<'_>,
    receipt: &JobReceipt,
) -> Result<(), WorkflowServiceError> {
    let outcome: ManagementOutcome = decode(&history.outcome)?;
    if history.id != command.job.id.as_str()
        || history.app_id != command.job.app_id.as_str()
        || history.run_id != command.run_id.as_str()
        || history.request_id != command.request_id.as_str()
        || history.revision != command.revision
        || history.digest != digest(command.job)?
        || receipt.outcome != (JobOutcome::Management { outcome })
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) async fn finish(
    tx: &Transaction,
    command: Command<'_>,
    previous: Option<&Scope>,
    outcome: ManagementOutcome,
    now: i64,
) -> Result<JobReceipt, WorkflowServiceError> {
    let pending = tx
        .database()
        .entity::<job_receipts::Entity>()?
        .insert::<_, delivery::Record>(PendingReceipt {
            id: command.job.id.as_str().to_owned(),
            app_id: command.job.app_id.as_str().to_owned(),
            specification: encode(command.job)?,
            created_at: now,
        })
        .await?;
    if pending.receipt(command.job)?.is_some() {
        return Err(invalid());
    }
    let inserted = tx
        .database()
        .entity::<receipts::Entity>()?
        .insert::<_, History>(History {
            id: command.job.id.as_str().to_owned(),
            app_id: command.job.app_id.as_str().to_owned(),
            run_id: command.run_id.as_str().to_owned(),
            request_id: command.request_id.as_str().to_owned(),
            revision: command.revision,
            digest: digest(command.job)?,
            outcome: encode(&outcome)?,
            created_at: now,
        })
        .await?;
    check_history(
        &inserted,
        command,
        &JobReceipt {
            job: command.job.clone(),
            outcome: JobOutcome::Management { outcome },
        },
    )?;
    let scopes = tx.database().entity::<scopes::Entity>()?;
    if let Some(previous) = previous {
        let changed = scopes
            .update_many(
                scopes::id
                    .eq(previous.id.as_str())?
                    .and(scopes::app_id.eq(command.job.app_id.as_str())?)
                    .and(scopes::run_id.eq(command.run_id.as_str())?)
                    .and(scopes::revision.eq(previous.revision)?),
                scopes::revision.set(command.revision)?,
            )
            .await?;
        if changed != 1 {
            return Err(invalid());
        }
    } else {
        let inserted = scopes
            .insert::<_, Scope>(Scope {
                id: storage_id(),
                app_id: command.job.app_id.as_str().to_owned(),
                run_id: command.run_id.as_str().to_owned(),
                revision: command.revision,
            })
            .await?;
        if inserted.app_id != command.job.app_id.as_str()
            || inserted.run_id != command.run_id.as_str()
            || inserted.revision != command.revision
            || typed_id::parse_with_prefix(&inserted.id, "wjr").is_err()
        {
            return Err(invalid());
        }
    }
    delivery::finish(tx, command.job, JobOutcome::Management { outcome }, now).await
}
