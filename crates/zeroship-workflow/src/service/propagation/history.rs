use super::{
    decode, delivery, invalid, models, obligation, one, publication, AppId, JobOperation,
    JobOutcome, JobReceipt, JobSpec, Kind, Obligation, Page, PageResult, Transaction,
    WorkflowServiceError, MAX_ROW_LIMIT,
};
use std::collections::BTreeSet;
use zeroship_core::workflow_jobs::JobId;

/// Fresh work follows the immutable previous page and its exact successor.
/// Cursors are compared only for equality with neighbouring pages.
pub(super) async fn validate_progress(
    tx: &Transaction,
    job: &JobSpec,
    current: &Obligation,
) -> Result<(), WorkflowServiceError> {
    if current.revision == 1 {
        return if current.cursor.is_none() {
            Ok(())
        } else {
            Err(invalid())
        };
    }
    let (previous, previous_job) =
        page_job(tx, &job.app_id, &current.id, current.revision - 1).await?;
    let receipt = receipt(tx, &previous_job).await?.ok_or_else(invalid)?;
    let result: PageResult = decode(&previous.result)?;
    if receipt.outcome != (JobOutcome::Waiting {})
        || result.after != current.cursor
        || !result.successors.contains(job)
    {
        return Err(invalid());
    }
    Ok(())
}

async fn page_job(
    tx: &Transaction,
    app: &AppId,
    propagation: &str,
    revision: i64,
) -> Result<(Page, JobSpec), WorkflowServiceError> {
    let page = tx
        .database()
        .entity::<models::propagation_pages::Entity>()?
        .find::<Page>(
            models::propagation_pages::app_id
                .eq(app.as_str())?
                .and(models::propagation_pages::propagation_id.eq(propagation)?)
                .and(models::propagation_pages::revision.eq(revision)?),
            one(),
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(invalid)?;
    let id = JobId::parse(&page.id).map_err(|_| invalid())?;
    let job = publication::intent_job(tx, app, &id).await?;
    if !matches!(&job.operation, JobOperation::Propagate { propagation_id, revision: page_revision }
        if propagation_id.as_str() == propagation && page_revision.get() == revision)
    {
        return Err(invalid());
    }
    Ok((page, job))
}

/// Exact committed page replay. Historical pages stay valid after later pages
/// advance the obligation; each is checked against its immediate successor.
pub(super) async fn receipt(
    tx: &Transaction,
    job: &JobSpec,
) -> Result<Option<JobReceipt>, WorkflowServiceError> {
    let JobOperation::Propagate {
        propagation_id,
        revision,
    } = &job.operation
    else {
        return Err(invalid());
    };
    let record = delivery::read(tx, job).await?;
    let page = tx
        .database()
        .entity::<models::propagation_pages::Entity>()?
        .find::<Page>(
            models::propagation_pages::id
                .eq(job.id.as_str())?
                .and(models::propagation_pages::app_id.eq(job.app_id.as_str())?),
            one(),
        )
        .await?
        .into_iter()
        .next();
    let (record, page) = match (record, page) {
        (None, None) => return Ok(None),
        (Some(record), Some(page)) => (record, page),
        _ => return Err(invalid()),
    };
    publication::exact(tx, job).await?;
    if page.id != job.id.as_str()
        || page.app_id != job.app_id.as_str()
        || page.propagation_id != propagation_id.as_str()
        || page.revision != revision.get()
    {
        return Err(invalid());
    }
    let result: PageResult = decode(&page.result)?;
    let (current, kind) = obligation(tx, &job.app_id, propagation_id).await?;
    let bound = usize::try_from(MAX_ROW_LIMIT).map_err(|_| invalid())?;
    if result.propagation != current.definition()?
        || result.propagation.kind != kind
        || result.revision != revision.get()
        || current.revision <= result.revision
        || result.affected < 0
        || result.affected > MAX_ROW_LIMIT
        || result.successors.len() > bound + 1
        || (result.superseded
            && (kind != Kind::Notify
                || !result.finished
                || result.affected != 0
                || result.before != result.after))
    {
        return Err(invalid());
    }
    if current.revision == result.revision + 1 {
        if current.cursor != result.after || (current.finished == 1) != result.finished {
            return Err(invalid());
        }
    } else {
        let (following, _) = page_job(
            tx,
            &job.app_id,
            propagation_id.as_str(),
            result.revision + 1,
        )
        .await?;
        let following: PageResult = decode(&following.result)?;
        if result.finished || following.before != result.after {
            return Err(invalid());
        }
    }
    let receipt = record.receipt(job)?.ok_or_else(invalid)?;
    let expected = if result.finished {
        JobOutcome::Completed {}
    } else {
        JobOutcome::Waiting {}
    };
    if receipt.outcome != expected {
        return Err(invalid());
    }
    successors(tx, job, &result).await?;
    Ok(Some(receipt))
}

/// Retained successors are the page's exact publications: Advance intents for
/// affected runs and, unless finished, the obligation's next page.
async fn successors(
    tx: &Transaction,
    job: &JobSpec,
    result: &PageResult,
) -> Result<(), WorkflowServiceError> {
    let JobOperation::Propagate { propagation_id, .. } = &job.operation else {
        return Err(invalid());
    };
    let mut next_pages = 0;
    let mut advances = 0_i64;
    let mut identities = BTreeSet::new();
    for successor in &result.successors {
        if successor.app_id != job.app_id || !identities.insert(successor.id.clone()) {
            return Err(invalid());
        }
        publication::exact(tx, successor).await?;
        match &successor.operation {
            JobOperation::Advance { .. } => advances += 1,
            JobOperation::Propagate {
                propagation_id: next_id,
                revision: next,
            } if next_id == propagation_id && next.get() == result.revision + 1 => {
                next_pages += 1;
            }
            _ => return Err(invalid()),
        }
    }
    if next_pages != usize::from(!result.finished) || advances > result.affected {
        return Err(invalid());
    }
    Ok(())
}
