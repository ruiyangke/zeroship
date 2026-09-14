use super::{
    by_id, invalid, models,
    read::generation_record,
    records::{Generation, Head, Membership},
    AppId, Member, Transaction, WorkflowServiceError,
};

/// Bind an inserted generation before publishing the originating transition.
pub async fn create(
    tx: &Transaction,
    app: &AppId,
    run: &str,
    generation: i64,
) -> Result<Member, WorkflowServiceError> {
    let generation = generation_record(tx, app, run, generation).await?;
    create_generation(tx, app, &generation).await
}

pub async fn advance(
    tx: &Transaction,
    app: &AppId,
    source: &Member,
    next_run: &str,
    next_generation: i64,
) -> Result<Member, WorkflowServiceError> {
    let current = by_id(tx, app, &source.id).await?;
    if !current.same_identity(source) || !current.is_current {
        return Err(invalid());
    }
    let next = generation_record(tx, app, next_run, next_generation).await?;
    advance_generation(tx, app, &current, next).await
}

/// Historical sources fork; restarting the head advances its existing chain.
/// Call before changing the source run pointer or its generation state.
pub async fn restart(
    tx: &Transaction,
    app: &AppId,
    source: &Member,
    next_run: &str,
    next_generation: i64,
) -> Result<Member, WorkflowServiceError> {
    let current = by_id(tx, app, &source.id).await?;
    if !current.same_identity(source)
        || next_run != source.run_id
        || Some(next_generation) != source.generation.checked_add(1)
    {
        return Err(invalid());
    }
    let next = generation_record(tx, app, next_run, next_generation).await?;
    if current.is_current {
        advance_generation(tx, app, &current, next).await
    } else {
        create_generation(tx, app, &next).await
    }
}

async fn create_generation(
    tx: &Transaction,
    app: &AppId,
    generation: &Generation,
) -> Result<Member, WorkflowServiceError> {
    require_unbound(tx, app, generation).await?;
    let head = Head {
        id: super::super::types::storage_id(),
        app_id: app.as_str().to_owned(),
        current_generation_id: generation.id.clone(),
        revision: 1,
    };
    let head = tx
        .database()
        .entity::<models::continuation_heads::Entity>()?
        .insert::<_, Head>(head)
        .await?;
    insert_member(tx, app, &head.id, head.revision, generation).await
}

async fn advance_generation(
    tx: &Transaction,
    app: &AppId,
    source: &Member,
    generation: Generation,
) -> Result<Member, WorkflowServiceError> {
    use models::continuation_heads as heads;
    require_unbound(tx, app, &generation).await?;
    let revision = source.revision.checked_add(1).ok_or_else(|| {
        WorkflowServiceError::ResourceExhausted("workflow continuation revision exhausted".into())
    })?;
    let next = insert_member(tx, app, &source.head_id, revision, &generation).await?;
    let changed = tx
        .database()
        .entity::<heads::Entity>()?
        .update_many(
            heads::app_id
                .eq(app.as_str())?
                .and(heads::id.eq(source.head_id.as_str())?)
                .and(heads::current_generation_id.eq(source.id.as_str())?)
                .and(heads::revision.eq(source.revision)?),
            heads::current_generation_id
                .set(generation.id)?
                .and(heads::revision.set(revision)?)?,
        )
        .await?;
    if changed != 1 {
        return Err(invalid());
    }
    Ok(next)
}

async fn insert_member(
    tx: &Transaction,
    app: &AppId,
    head: &str,
    revision: i64,
    generation: &Generation,
) -> Result<Member, WorkflowServiceError> {
    let record = Membership {
        id: generation.id.clone(),
        app_id: app.as_str().to_owned(),
        head_id: head.to_owned(),
        revision,
    };
    let identity = record.identity(app, generation)?.observed(true);
    tx.database()
        .entity::<models::continuation_members::Entity>()?
        .insert::<_, Membership>(record)
        .await?;
    Ok(identity)
}

async fn require_unbound(
    tx: &Transaction,
    app: &AppId,
    generation: &Generation,
) -> Result<(), WorkflowServiceError> {
    generation.validate(app)?;
    let bound = tx
        .database()
        .entity::<models::continuation_members::Entity>()?
        .exists(
            models::continuation_members::app_id
                .eq(app.as_str())?
                .and(models::continuation_members::id.eq(generation.id.as_str())?),
        )
        .await?;
    let headed = tx
        .database()
        .entity::<models::continuation_heads::Entity>()?
        .exists(
            models::continuation_heads::app_id
                .eq(app.as_str())?
                .and(models::continuation_heads::current_generation_id.eq(generation.id.as_str())?),
        )
        .await?;
    if bound || headed {
        return Err(invalid());
    }
    Ok(())
}
