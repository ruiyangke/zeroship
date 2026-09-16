use super::{
    conflict, delivery, invalid, Command, JobReceipt, JobSpec, Transaction, WorkflowServiceError,
};
use crate::service::{
    app::{decode, encode},
    models::{job_receipts, management_receipts as receipts},
};
use zeroship_core::{
    app_id::AppId, workflow_coordination::ManagementOutcome, workflow_jobs::JobOutcome,
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
    outcome: String,
    created_at: i64,
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
    /// The run's applied management revision, absent until its first command.
    Fresh(Option<i64>),
}

impl Observed {
    pub(super) fn require_next(self, revision: i64) -> Result<(), WorkflowServiceError> {
        let Self::Fresh(previous) = self else {
            return Err(invalid());
        };
        let expected = previous.unwrap_or(0).checked_add(1).ok_or_else(|| {
            WorkflowServiceError::ResourceExhausted("workflow management revision exhausted".into())
        })?;
        if revision != expected {
            return Err(conflict());
        }
        Ok(())
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
    let current = applied(tx, &command.job.app_id, command.run_id.as_str()).await?;
    if let Some(receipt) = recorded {
        if anchored.len() != 1 {
            return Err(invalid());
        }
        let history = &anchored[0];
        check_history(history, command, &receipt)?;
        if current.is_none_or(|revision| revision < command.revision) {
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

/// The management revision this app's run has applied: the highest revision its
/// retained command history carries. One journal holds many apps, so the app
/// bounds the lookup as tightly as the run does, and the revision identity index
/// over both orders it. Reading it validates the head command it names.
async fn applied(
    tx: &Transaction,
    app: &AppId,
    run: &str,
) -> Result<Option<i64>, WorkflowServiceError> {
    let database = tx.database();
    let source = database.entity::<receipts::Entity>()?.alias("h")?;
    let head = database
        .from(&source)
        .filter(
            source
                .column(receipts::app_id)
                .eq(app.as_str())?
                .and(source.column(receipts::run_id).eq(run)?),
        )
        .order_by(source.column(receipts::revision).desc())
        .select(source.row::<History>())?
        .limit(1)?
        .all()
        .await?
        .into_iter()
        .next();
    let Some(head) = head else {
        return Ok(None);
    };
    if head.revision <= 0 {
        return Err(invalid());
    }
    validate_saved(tx, &head).await?;
    Ok(Some(head.revision))
}

/// Re-derive a retained command from the immutable specification its receipt
/// holds, and require the history row's projections and outcome to be the ones
/// that specification and its settled receipt carry.
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
        || receipt.outcome != (JobOutcome::Management { outcome })
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) async fn finish(
    tx: &Transaction,
    command: Command<'_>,
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
    delivery::finish(tx, command.job, JobOutcome::Management { outcome }, now).await
}
