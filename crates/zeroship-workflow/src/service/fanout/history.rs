use super::{
    broadcast, decode, delivery, invalid, models, one, publication, topic_record, AppId, Broadcast,
    JobOperation, JobOutcome, JobReceipt, JobSpec, Page, PageResult, TopicRecord, Transaction,
    WorkflowServiceError,
};

/// Fresh work follows the immutable previous page and completed topic head.
/// Historical receipt replay remains valid after both current heads advance.
pub(super) async fn validate_progress(
    tx: &Transaction,
    job: &JobSpec,
    current: &Broadcast,
    topic: &TopicRecord,
) -> Result<(), WorkflowServiceError> {
    if current.revision == 1 {
        if current.cursor != 0 {
            return Err(invalid());
        }
    } else {
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
    }
    if tx
        .database()
        .entity::<models::broadcasts::Entity>()?
        .exists(
            models::broadcasts::app_id
                .eq(job.app_id.as_str())?
                .and(models::broadcasts::topic.eq(current.topic.as_str())?)
                .and(models::broadcasts::finished.eq(1_i64)?)
                .and(models::broadcasts::sequence.gt(topic.completed_sequence)?),
        )
        .await?
    {
        return Err(invalid());
    }
    if topic.completed_sequence == 0 {
        return Ok(());
    }
    let completed = tx
        .database()
        .entity::<models::broadcasts::Entity>()?
        .find::<Broadcast>(
            models::broadcasts::app_id
                .eq(job.app_id.as_str())?
                .and(models::broadcasts::topic.eq(current.topic.as_str())?)
                .and(models::broadcasts::sequence.eq(topic.completed_sequence)?),
            one(),
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(invalid)?;
    if completed.finished != 1 || completed.revision <= 1 {
        return Err(invalid());
    }
    let (_, completed_job) =
        page_job(tx, &job.app_id, &completed.id, completed.revision - 1).await?;
    if receipt(tx, &completed_job)
        .await?
        .ok_or_else(invalid)?
        .outcome
        != (JobOutcome::Completed {})
    {
        return Err(invalid());
    }
    Ok(())
}

async fn page_job(
    tx: &Transaction,
    app: &AppId,
    broadcast: &str,
    revision: i64,
) -> Result<(Page, JobSpec), WorkflowServiceError> {
    let page = tx
        .database()
        .entity::<models::fanout_pages::Entity>()?
        .find::<Page>(
            models::fanout_pages::app_id
                .eq(app.as_str())?
                .and(models::fanout_pages::broadcast_id.eq(broadcast)?)
                .and(models::fanout_pages::revision.eq(revision)?),
            one(),
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(invalid)?;
    let id = zeroship_core::workflow_jobs::JobId::parse(&page.id).map_err(|_| invalid())?;
    let job = publication::intent_job(tx, app, &id).await?;
    if !matches!(&job.operation, JobOperation::Fanout { broadcast_id, revision: page_revision } if broadcast_id.as_str() == broadcast && page_revision.get() == revision)
    {
        return Err(invalid());
    }
    Ok((page, job))
}

pub(super) async fn receipt(
    tx: &Transaction,
    job: &JobSpec,
) -> Result<Option<JobReceipt>, WorkflowServiceError> {
    let JobOperation::Fanout {
        broadcast_id,
        revision,
    } = &job.operation
    else {
        return Err(invalid());
    };
    let record = delivery::read(tx, job).await?;
    let page = tx
        .database()
        .entity::<models::fanout_pages::Entity>()?
        .find::<Page>(
            models::fanout_pages::id
                .eq(job.id.as_str())?
                .and(models::fanout_pages::app_id.eq(job.app_id.as_str())?),
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
        || page.broadcast_id != broadcast_id.as_str()
        || page.revision != revision.get()
    {
        return Err(invalid());
    }
    let result: PageResult = decode(&page.result)?;
    let current = broadcast(tx, &job.app_id, broadcast_id).await?;
    let topic = topic_record(tx, &job.app_id, &current.topic)
        .await?
        .ok_or_else(invalid)?;
    let finished = current.finished == 1;
    let completed_by_topic = current.sequence <= topic.completed_sequence;
    if result.broadcast != current.definition()
        || result.revision != revision.get()
        || result.before < 0
        || result.after < result.before
        || result.after > current.cutoff_sequence
        || result.delivered < 0
        || result.delivered > result.after - result.before
        || result.successors.len() > 1025
        || current.revision <= result.revision
        || current.cursor < result.after
        || current.sequence > topic.accepted_sequence
        || finished != completed_by_topic
        || (result.finished && (!finished || !completed_by_topic))
    {
        return Err(invalid());
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
    let mut next_pages = 0;
    let mut identities = std::collections::BTreeSet::new();
    for successor in &result.successors {
        if successor.app_id != job.app_id || !identities.insert(successor.id.clone()) {
            return Err(invalid());
        }
        publication::exact(tx, successor).await?;
        match &successor.operation {
            JobOperation::Advance { .. } => {}
            JobOperation::Fanout {
                broadcast_id: next_id,
                revision: next,
            } if next_id == broadcast_id && next.get() == result.revision + 1 => next_pages += 1,
            _ => return Err(invalid()),
        }
    }
    if next_pages != usize::from(!result.finished) {
        return Err(invalid());
    }
    Ok(Some(receipt))
}
