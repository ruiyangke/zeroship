use super::{
    invalid, models, one,
    records::{journal_id, ChildStep, Generation, HeadIdentity, Membership},
    sources::{Joined, Sources},
    AppId, HistoricalMember, Member, ResolvedChild, Transaction, WorkflowServiceError,
};
use zeroship_data_orm::orm::FromRow;

#[derive(FromRow)]
#[orm(entity = models::waits)]
struct Wait {
    id: String,
    kind: String,
}

/// A parent wait whose checkpoint accepted a member of a completing head.
#[derive(FromRow)]
#[orm(entity = models::waits)]
pub struct WaitingParent {
    pub id: String,
    pub run_id: String,
    pub generation: i64,
    pub ordinal: i64,
}

/// One page of current parent waits on `child`'s head, after wait `after`.
///
/// The page starts from the head's members and follows each accepted
/// checkpoint reference, so its cost is bounded by the head's own waiters.
/// Each row is validated in memory by the rules [`resolve`] applies, without
/// a read per parent.
pub async fn waiting(
    tx: &Transaction,
    app: &AppId,
    child: &Member,
    after: Option<&str>,
    limit: i64,
) -> Result<Vec<WaitingParent>, WorkflowServiceError> {
    use models::{continuation_members as m, generations as g, runs as r, steps as s, waits as w};
    tx.check_app(app)?;
    if !child.is_current {
        return Err(invalid());
    }
    let db = tx.database();
    let member = db.entity::<m::Entity>()?.alias("accepted")?;
    let generation = db.entity::<g::Entity>()?.alias("accepted_generation")?;
    let step = db.entity::<s::Entity>()?.alias("parent_step")?;
    let wait = db.entity::<w::Entity>()?.alias("parent_wait")?;
    let run = db.entity::<r::Entity>()?.alias("parent_run")?;
    let mut filter = member
        .column(m::app_id)
        .eq(app.as_str())?
        .and(member.column(m::head_id).eq(child.head_id.as_str())?)
        .and(wait.column(w::kind).eq("child")?);
    if let Some(after) = after {
        filter = filter.and(wait.column(w::id).gt(after)?);
    }
    let rows = db
        .from(&member)
        .inner_join(
            &generation,
            member
                .column(m::app_id)
                .eq(generation.column(g::app_id))?
                .and(member.column(m::id).eq(generation.column(g::id))?),
        )?
        .inner_join(
            &step,
            member
                .column(m::app_id)
                .eq(step.column(s::app_id))?
                .and(member.column(m::id).eq(step.column(s::child_member_id))?),
        )?
        .inner_join(
            &wait,
            step.column(s::app_id)
                .eq(wait.column(w::app_id))?
                .and(step.column(s::run_id).eq(wait.column(w::run_id))?)
                .and(step.column(s::generation).eq(wait.column(w::generation))?)
                .and(step.column(s::ordinal).eq(wait.column(w::ordinal))?),
        )?
        .inner_join(
            &run,
            wait.column(w::app_id)
                .eq(run.column(r::app_id))?
                .and(wait.column(w::run_id).eq(run.column(r::id))?)
                .and(wait.column(w::generation).eq(run.column(r::generation))?),
        )?
        .filter(filter)
        .order_by(wait.column(w::id).asc())
        .select((
            wait.row::<WaitingParent>(),
            step.row::<ChildStep>(),
            member.row::<Membership>(),
            generation.row::<Generation>(),
        ))?
        .limit(limit)?
        .all()
        .await?;
    rows.into_iter()
        .map(|(parent, step, membership, generation)| {
            let accepted = membership.identity(app, &generation)?;
            journal_id(&parent.id)?;
            if accepted.head_id != child.head_id
                || accepted.revision > child.revision
                || step.run_id != parent.run_id
                || step.generation != parent.generation
                || step.ordinal != parent.ordinal
            {
                return Err(invalid());
            }
            let is_current = accepted.id == child.id;
            validate_step(app, &step, &accepted.observed(is_current))?;
            Ok(parent)
        })
        .collect()
}

pub async fn member(
    tx: &Transaction,
    app: &AppId,
    run: &str,
    generation: i64,
) -> Result<Member, WorkflowServiceError> {
    let generation = generation_record(tx, app, run, generation).await?;
    let result = by_id(tx, app, &generation.id).await?;
    if result.run_id != run || result.generation != generation.generation {
        return Err(invalid());
    }
    Ok(result)
}

pub async fn by_id(
    tx: &Transaction,
    app: &AppId,
    id: &str,
) -> Result<Member, WorkflowServiceError> {
    Ok(read(tx, app, id).await?.accepted)
}

/// Read immutable membership without consulting the mutable head target or run.
pub async fn historical(
    tx: &Transaction,
    app: &AppId,
    id: &str,
) -> Result<HistoricalMember, WorkflowServiceError> {
    tx.check_app(app)?;
    journal_id(id)?;
    let db = tx.database();
    let sources = Sources::new(db)?;
    let (membership, generation, head) = sources
        .base(db)?
        .filter(
            sources
                .accepted
                .column(models::continuation_members::app_id)
                .eq(app.as_str())?
                .and(
                    sources
                        .accepted
                        .column(models::continuation_members::id)
                        .eq(id)?,
                ),
        )
        .select((
            sources.accepted.row::<Membership>(),
            sources.accepted_generation.row::<Generation>(),
            sources.head.row::<HeadIdentity>(),
        ))?
        .limit(1)?
        .all()
        .await?
        .into_iter()
        .next()
        .ok_or_else(invalid)?;
    let identity = membership.identity(app, &generation)?;
    journal_id(&head.id)?;
    if head.app_id != app.as_str() || identity.head_id != head.id {
        return Err(invalid());
    }
    Ok(identity)
}

pub async fn resolve(
    tx: &Transaction,
    app: &AppId,
    parent: &str,
    generation: i64,
    ordinal: i64,
) -> Result<ResolvedChild, WorkflowServiceError> {
    use models::{continuation_members as m, steps as s, waits as w};
    tx.check_app(app)?;
    let db = tx.database();
    let sources = Sources::new(db)?;
    let step = db.entity::<models::steps::Entity>()?.alias("child_step")?;
    let wait = db.entity::<models::waits::Entity>()?.alias("child_wait")?;
    let (joined, checkpoint, waiting) = sources
        .read(db)?
        .inner_join(
            &step,
            sources
                .accepted
                .column(m::app_id)
                .eq(step.column(s::app_id))?
                .and(
                    step.column(s::child_member_id)
                        .eq(sources.accepted.column(m::id))?,
                ),
        )?
        .inner_join(
            &wait,
            step.column(s::app_id)
                .eq(wait.column(w::app_id))?
                .and(step.column(s::run_id).eq(wait.column(w::run_id))?)
                .and(step.column(s::generation).eq(wait.column(w::generation))?)
                .and(step.column(s::ordinal).eq(wait.column(w::ordinal))?),
        )?
        .filter(
            step.column(s::app_id)
                .eq(app.as_str())?
                .and(step.column(s::run_id).eq(parent)?)
                .and(step.column(s::generation).eq(generation)?)
                .and(step.column(s::ordinal).eq(ordinal)?),
        )
        .select((
            sources.selection(),
            step.row::<ChildStep>(),
            wait.row::<Wait>(),
        ))?
        .limit(1)?
        .all()
        .await?
        .into_iter()
        .next()
        .ok_or_else(invalid)?;
    let result = validate(app, joined)?;
    journal_id(&waiting.id)?;
    if waiting.kind != "child"
        || checkpoint.run_id != parent
        || checkpoint.generation != generation
        || checkpoint.ordinal != ordinal
    {
        return Err(invalid());
    }
    validate_step(app, &checkpoint, &result.accepted)?;
    Ok(result)
}

fn validate_step(
    app: &AppId,
    step: &ChildStep,
    accepted: &Member,
) -> Result<(), WorkflowServiceError> {
    let record = super::super::journal::decode_checkpoint(
        &step.record,
        step.child_member_id.as_deref(),
        step.child_result_member_id.as_deref(),
    )?;
    journal_id(&step.id)?;
    if step.app_id != app.as_str()
        || step.kind != "child"
        || step.state != "running"
        || step.child_member_id.as_deref() != Some(accepted.id.as_str())
        || step.child_result_member_id.is_some()
        || record.kind != "child"
        || record.state != "running"
        || i64::from(record.ordinal) != step.ordinal
        || record.child_run_id.as_deref() != Some(accepted.run_id.as_str())
    {
        return Err(invalid());
    }
    Ok(())
}

async fn read(
    tx: &Transaction,
    app: &AppId,
    id: &str,
) -> Result<ResolvedChild, WorkflowServiceError> {
    tx.check_app(app)?;
    journal_id(id)?;
    let db = tx.database();
    let sources = Sources::new(db)?;
    let joined = sources
        .read(db)?
        .filter(
            sources
                .accepted
                .column(models::continuation_members::app_id)
                .eq(app.as_str())?
                .and(
                    sources
                        .accepted
                        .column(models::continuation_members::id)
                        .eq(id)?,
                ),
        )
        .select(sources.selection())?
        .limit(1)?
        .all()
        .await?
        .into_iter()
        .next()
        .ok_or_else(invalid)?;
    validate(app, joined)
}

fn validate(app: &AppId, joined: Joined) -> Result<ResolvedChild, WorkflowServiceError> {
    let (accepted, accepted_generation, head, current, generation, run) = joined;
    let accepted = accepted.identity(app, &accepted_generation)?;
    let current = current.identity(app, &generation)?;
    journal_id(&head.id)?;
    journal_id(&head.current_generation_id)?;
    if head.app_id != app.as_str()
        || accepted.head_id != head.id
        || current.head_id != head.id
        || head.revision <= 0
        || accepted.revision > head.revision
        || current.revision != head.revision
        || current.id != head.current_generation_id
        || run.app_id != app.as_str()
        || run.id != generation.run_id
        || run.generation != generation.generation
    {
        return Err(invalid());
    }
    let state = super::super::app::parse_state(&run.state)?;
    if generation.state == "restarted"
        || state.is_terminal() != super::super::app::parse_state(&generation.state)?.is_terminal()
        || (state.is_terminal() && run.state != generation.state)
    {
        return Err(invalid());
    }
    let is_current = accepted.id == current.id;
    Ok(ResolvedChild {
        accepted: accepted.observed(is_current),
        current: current.observed(true),
        state,
        outcome: generation.outcome(),
    })
}

pub(super) async fn generation_record(
    tx: &Transaction,
    app: &AppId,
    run: &str,
    generation: i64,
) -> Result<Generation, WorkflowServiceError> {
    tx.check_app(app)?;
    let record = tx
        .database()
        .entity::<models::generations::Entity>()?
        .find::<Generation>(
            models::generations::app_id
                .eq(app.as_str())?
                .and(models::generations::run_id.eq(run)?)
                .and(models::generations::generation.eq(generation)?),
            one(),
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(invalid)?;
    record.validate(app)?;
    Ok(record)
}
