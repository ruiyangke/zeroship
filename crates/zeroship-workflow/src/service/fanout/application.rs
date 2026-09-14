use super::{
    broadcast, changed, delivery, encode, history, increment, invalid, models, publication,
    recipients, signals, topic_record, AppId, Broadcast, FanoutOptions, JobOperation, JobOutcome,
    JobReceipt, JobSpec, Page, PageResult, Pending, TopicRecord, Transaction, WorkflowServiceError,
};

pub(super) async fn apply(
    tx: &mut Transaction,
    job: &JobSpec,
    options: FanoutOptions,
) -> Result<Option<JobReceipt>, WorkflowServiceError> {
    publication::exact(tx, job).await?;
    let JobOperation::Fanout {
        broadcast_id,
        revision,
    } = &job.operation
    else {
        return Err(invalid());
    };
    let broadcast = broadcast(tx, &job.app_id, broadcast_id).await?;
    let topic = topic_record(tx, &job.app_id, &broadcast.topic)
        .await?
        .ok_or_else(invalid)?;
    history::validate_progress(tx, job, &broadcast, &topic).await?;
    if broadcast.sequence > topic.accepted_sequence || broadcast.revision < revision.get() {
        return Err(invalid());
    }
    if broadcast.revision != revision.get()
        || broadcast.finished != 0
        || broadcast.sequence <= topic.completed_sequence
    {
        return Err(invalid());
    }
    if broadcast.sequence
        != increment(
            topic.completed_sequence,
            "workflow broadcast completion exhausted",
        )?
    {
        return Ok(None);
    }
    let now = tx.now().await?;
    let recipients = recipients::select(tx, &job.app_id, &broadcast, options.page_size).await?;
    let mut result = PageResult {
        broadcast: broadcast.definition(),
        revision: revision.get(),
        before: broadcast.cursor,
        after: broadcast.cursor,
        finished: recipients.len() < options.page_size as usize,
        delivered: 0,
        successors: Vec::new(),
    };
    for recipient in recipients {
        result.after = recipient.sequence;
        if Box::pin(signals::materialize(
            tx,
            &job.app_id,
            &broadcast,
            &recipient,
            now,
        ))
        .await?
        {
            result.delivered += 1;
            if let Some(job) = Box::pin(wake(tx, &job.app_id, &recipient, now)).await? {
                result.successors.push(job);
            }
        }
    }
    Box::pin(persist(tx, job, &broadcast, &topic, &mut result, now))
        .await
        .map(Some)
}

async fn persist(
    tx: &Transaction,
    job: &JobSpec,
    broadcast: &Broadcast,
    topic: &TopicRecord,
    result: &mut PageResult,
    now: i64,
) -> Result<JobReceipt, WorkflowServiceError> {
    let JobOperation::Fanout {
        broadcast_id,
        revision,
    } = &job.operation
    else {
        return Err(invalid());
    };
    let next = increment(revision.get(), "workflow fanout revision exhausted")?;
    changed(
        tx.database()
            .entity::<models::broadcasts::Entity>()?
            .update_many(
                models::broadcasts::app_id
                    .eq(job.app_id.as_str())?
                    .and(models::broadcasts::id.eq(broadcast_id.as_str())?)
                    .and(models::broadcasts::revision.eq(revision.get())?),
                models::broadcasts::cursor
                    .set(result.after)?
                    .and(models::broadcasts::finished.set(i64::from(result.finished))?)?
                    .and(models::broadcasts::revision.set(next)?)?,
            )
            .await?,
    )?;
    if result.finished {
        changed(
            tx.database()
                .entity::<models::topics::Entity>()?
                .update_many(
                    models::topics::id
                        .eq(topic.id.as_str())?
                        .and(models::topics::completed_sequence.eq(topic.completed_sequence)?),
                    models::topics::completed_sequence.set(broadcast.sequence)?,
                )
                .await?,
        )?;
    } else {
        result.successors.push(
            publication::fanout(
                tx,
                &job.app_id,
                broadcast_id,
                next.try_into().map_err(|_| invalid())?,
                now,
            )
            .await?,
        );
    }
    let pending = tx
        .database()
        .entity::<models::job_receipts::Entity>()?
        .insert::<_, delivery::Record>(Pending {
            id: job.id.as_str().to_owned(),
            app_id: job.app_id.as_str().to_owned(),
            specification: encode(job)?,
            created_at: now,
        })
        .await?;
    if pending.receipt(job)?.is_some() {
        return Err(invalid());
    }
    tx.database()
        .entity::<models::fanout_pages::Entity>()?
        .insert::<_, Page>(Page {
            id: job.id.as_str().to_owned(),
            app_id: job.app_id.as_str().to_owned(),
            broadcast_id: broadcast_id.as_str().to_owned(),
            revision: revision.get(),
            result: encode(result)?,
        })
        .await?;
    let outcome = if result.finished {
        JobOutcome::Completed {}
    } else {
        JobOutcome::Waiting {}
    };
    delivery::finish(tx, job, outcome, now).await?;
    history::receipt(tx, job).await?.ok_or_else(invalid)
}

async fn wake(
    tx: &Transaction,
    app: &AppId,
    recipient: &models::SubscriptionRecipient,
    now: i64,
) -> Result<Option<JobSpec>, WorkflowServiceError> {
    let changed = tx
        .database()
        .entity::<models::runs::Entity>()?
        .update_many(
            models::runs::app_id
                .eq(app.as_str())?
                .and(models::runs::id.eq(recipient.run_id.as_str())?)
                .and(models::runs::generation.eq(recipient.generation)?)
                .and(models::runs::task_id.eq(None::<&str>)?)
                .and(models::runs::control.eq("none")?)
                .and(models::runs::state.eq("waiting")?),
            models::runs::due_at.set(Some(now))?,
        )
        .await?;
    if changed == 0 {
        return Ok(None);
    }
    if changed != 1 {
        return Err(invalid());
    }
    Box::pin(publication::advance(tx, app, &recipient.run_id, now)).await?;
    publication::record_job(tx, app, &recipient.run_id, now).await
}
