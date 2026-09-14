use super::{
    changed, continuations, delivery, encode, history, increment, invalid, models, obligation,
    parse_state, publication, revision, AppId, JobOperation, JobOutcome, JobReceipt, JobSpec, Kind,
    Obligation, Page, PageResult, Pending, PropagationOptions, Transaction, WorkflowServiceError,
};
use std::collections::BTreeSet;
use zeroship_core::workflow_coordination::RunId;
use zeroship_data_orm::orm::FromRow;

#[derive(FromRow)]
#[orm(entity = models::runs)]
struct Child {
    id: String,
    state: String,
    task_id: Option<String>,
}

pub(super) async fn apply(
    tx: &mut Transaction,
    job: &JobSpec,
    options: PropagationOptions,
) -> Result<JobReceipt, WorkflowServiceError> {
    publication::exact(tx, job).await?;
    let JobOperation::Propagate {
        propagation_id,
        revision: page,
    } = &job.operation
    else {
        return Err(invalid());
    };
    let (current, kind) = obligation(tx, &job.app_id, propagation_id).await?;
    if current.revision != page.get() || current.finished != 0 {
        return Err(invalid());
    }
    history::validate_progress(tx, job, &current).await?;
    let now = tx.now().await?;
    let mut result = PageResult {
        propagation: current.definition()?,
        revision: page.get(),
        before: current.cursor.clone(),
        after: current.cursor.clone(),
        finished: false,
        superseded: false,
        affected: 0,
        successors: Vec::new(),
    };
    match kind {
        Kind::Cascade => {
            Box::pin(cascade(
                tx,
                &job.app_id,
                &current,
                options,
                now,
                &mut result,
            ))
            .await?;
        }
        Kind::Notify => {
            Box::pin(notify(tx, &job.app_id, &current, options, now, &mut result)).await?;
        }
    }
    Box::pin(persist(tx, job, &mut result, now)).await
}

/// Record cancellation intent on the next page of the source generation's
/// cascading children. Leased children observe it at renewal or completion.
async fn cascade(
    tx: &Transaction,
    app: &AppId,
    source: &Obligation,
    options: PropagationOptions,
    now: i64,
    result: &mut PageResult,
) -> Result<(), WorkflowServiceError> {
    use models::runs;
    let db = tx.database();
    let run = db.entity::<runs::Entity>()?.alias("r")?;
    let mut filter = run
        .column(runs::app_id)
        .eq(app.as_str())?
        .and(
            run.column(runs::parent_id)
                .eq(Some(source.run_id.as_str()))?,
        )
        .and(
            run.column(runs::parent_generation)
                .eq(Some(source.generation))?,
        )
        .and(run.column(runs::cascade).eq(1_i64)?);
    if let Some(cursor) = &source.cursor {
        filter = filter.and(run.column(runs::id).gt(cursor.as_str())?);
    }
    let children = db
        .from(&run)
        .filter(filter)
        .order_by(run.column(runs::id).asc())
        .select(run.row::<Child>())?
        .limit(i64::from(options.page_size))?
        .all()
        .await?;
    result.finished = children.len() < options.page_size as usize;
    for child in children {
        RunId::parse(&child.id).map_err(|_| invalid())?;
        result.after = Some(child.id.clone());
        if parse_state(&child.state)?.is_terminal() {
            continue;
        }
        result.affected += 1;
        let entity = tx.database().entity::<runs::Entity>()?;
        let selected = runs::app_id
            .eq(app.as_str())?
            .and(runs::id.eq(child.id.as_str())?)
            .and(runs::task_id.eq(child.task_id.as_deref())?);
        if child.task_id.is_some() {
            changed(
                entity
                    .update_many(selected, runs::control.set("cancel")?)
                    .await?,
            )?;
            continue;
        }
        changed(
            entity
                .update_many(
                    selected,
                    runs::control
                        .set("cancel")?
                        .and(runs::due_at.set(Some(now))?)?,
                )
                .await?,
        )?;
        if let Some(advance) = Box::pin(publication::advance_job(tx, app, &child.id, now)).await? {
            result.successors.push(advance);
        }
    }
    Ok(())
}

/// Wake the next page of idle parents waiting on the source head. A head that
/// no longer names the terminal source generation supersedes the obligation.
async fn notify(
    tx: &Transaction,
    app: &AppId,
    source: &Obligation,
    options: PropagationOptions,
    now: i64,
    result: &mut PageResult,
) -> Result<(), WorkflowServiceError> {
    use models::runs;
    let member = continuations::member(tx, app, &source.run_id, source.generation).await?;
    if !member.is_current {
        result.finished = true;
        result.superseded = true;
        return Ok(());
    }
    let (run, _) = super::super::app::current_run(tx, app, &source.run_id)
        .await?
        .ok_or_else(invalid)?;
    if run.generation != source.generation || !parse_state(&run.state)?.is_terminal() {
        return Err(invalid());
    }
    let parents = continuations::waiting(
        tx,
        app,
        &member,
        source.cursor.as_deref(),
        i64::from(options.page_size),
    )
    .await?;
    result.finished = parents.len() < options.page_size as usize;
    let mut visited = BTreeSet::new();
    for parent in parents {
        result.after = Some(parent.id.clone());
        // A parent with several waits on this head is woken once per page.
        if !visited.insert(parent.run_id.clone()) {
            continue;
        }
        let woken = tx
            .database()
            .entity::<runs::Entity>()?
            .update_many(
                runs::app_id
                    .eq(app.as_str())?
                    .and(runs::id.eq(parent.run_id.as_str())?)
                    .and(runs::generation.eq(parent.generation)?)
                    .and(runs::task_id.eq(None::<&str>)?)
                    .and(runs::control.eq("none")?)
                    .and(runs::state.in_values(["waiting", "sleeping", "queued"])?),
                runs::due_at.set(Some(now))?,
            )
            .await?;
        if woken == 0 {
            continue;
        }
        changed(woken)?;
        result.affected += 1;
        if let Some(advance) =
            Box::pin(publication::advance_job(tx, app, &parent.run_id, now)).await?
        {
            result.successors.push(advance);
        }
    }
    Ok(())
}

async fn persist(
    tx: &Transaction,
    job: &JobSpec,
    result: &mut PageResult,
    now: i64,
) -> Result<JobReceipt, WorkflowServiceError> {
    use models::propagations;
    let JobOperation::Propagate {
        propagation_id,
        revision: page,
    } = &job.operation
    else {
        return Err(invalid());
    };
    let next = increment(page.get())?;
    changed(
        tx.database()
            .entity::<propagations::Entity>()?
            .update_many(
                propagations::app_id
                    .eq(job.app_id.as_str())?
                    .and(propagations::id.eq(propagation_id.as_str())?)
                    .and(propagations::revision.eq(page.get())?)
                    .and(propagations::finished.eq(0_i64)?),
                propagations::cursor
                    .set(result.after.clone())?
                    .and(propagations::finished.set(i64::from(result.finished))?)?
                    .and(propagations::revision.set(next)?)?,
            )
            .await?,
    )?;
    if !result.finished {
        result.successors.push(
            publication::propagate(tx, &job.app_id, propagation_id, revision(next)?, now).await?,
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
        .entity::<models::propagation_pages::Entity>()?
        .insert::<_, Page>(Page {
            id: job.id.as_str().to_owned(),
            app_id: job.app_id.as_str().to_owned(),
            propagation_id: propagation_id.as_str().to_owned(),
            revision: page.get(),
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
