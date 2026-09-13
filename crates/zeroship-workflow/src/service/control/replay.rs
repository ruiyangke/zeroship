//! Retain the selected restart prefix without changing its recorded effect origin.

use super::super::{models, store::Transaction};
use crate::WorkflowServiceError;
use zeroship_core::app_id::AppId;
use zeroship_data_orm::{
    budgets::MAX_INSERT_MANY_BATCH,
    orm::{Entity, FromRow, Insertable, Operation},
    sql::RowLimit,
    Value,
};

#[derive(FromRow, Insertable)]
#[orm(entity = models::steps)]
struct Checkpoint {
    id: String,
    app_id: String,
    run_id: String,
    generation: i64,
    ordinal: i64,
    name: String,
    occurrence: i64,
    origin_generation: i64,
    kind: String,
    state: String,
    record: String,
    compensation_attempts: i64,
    compensation_due_at: Option<i64>,
    compensation_error: Option<String>,
    compensation_retry_ms: i64,
}

#[derive(FromRow, Insertable)]
#[orm(entity = models::payload_refs)]
struct PayloadReference {
    id: String,
    app_id: String,
    run_id: String,
    generation: i64,
    slot: String,
    ordinal: i64,
    payload_id: String,
}

/// The caller owns the app and run locks and has created the target generation.
/// Reads and writes share its transaction; a failed page rolls back the restart.
pub(super) async fn copy_prefix(
    tx: &Transaction,
    app: &AppId,
    run: &str,
    source_generation: i64,
    target_generation: i64,
    prefix: i32,
) -> Result<(), WorkflowServiceError> {
    let db = tx.database();
    let steps = db.entity::<models::steps::Entity>()?.alias("s")?;
    let page_limit = RowLimit::default().get().min(MAX_INSERT_MANY_BATCH as i64);
    let mut after = None;
    loop {
        let mut predicates = steps
            .column(models::steps::app_id)
            .eq(app.as_str())?
            .and(steps.column(models::steps::run_id).eq(run)?)
            .and(
                steps
                    .column(models::steps::generation)
                    .eq(source_generation)?,
            )
            .and(steps.column(models::steps::ordinal).lt(i64::from(prefix))?);
        if let Some(after) = after {
            predicates = predicates.and(steps.column(models::steps::ordinal).gt(after)?);
        }
        let page = db
            .from(&steps)
            .filter(predicates)
            .order_by(steps.column(models::steps::ordinal).asc())
            .select(steps.row::<Checkpoint>())?
            .limit(page_limit)?
            .all()
            .await?;
        let count = page.len();
        let mut documents = Vec::with_capacity(count);
        for mut row in page {
            after = Some(row.ordinal);
            row.id = super::super::types::storage_id();
            row.generation = target_generation;
            documents.push(Value::Object(row.into_record()?));
        }
        if !documents.is_empty() {
            db.collection(models::steps::Entity::COLLECTION)?
                .execute(Operation::InsertMany {
                    documents: Value::Array(documents),
                })
                .await?;
        }
        if count < page_limit as usize {
            break;
        }
    }

    let refs = db.entity::<models::payload_refs::Entity>()?.alias("p")?;
    let mut after: Option<(String, i64)> = None;
    loop {
        let mut predicates = refs
            .column(models::payload_refs::app_id)
            .eq(app.as_str())?
            .and(refs.column(models::payload_refs::run_id).eq(run)?)
            .and(
                refs.column(models::payload_refs::generation)
                    .eq(source_generation)?,
            )
            .and(
                refs.column(models::payload_refs::slot).eq("input")?.or(refs
                    .column(models::payload_refs::slot)
                    .eq("step")?
                    .and(
                        refs.column(models::payload_refs::ordinal)
                            .lt(i64::from(prefix))?,
                    )),
            );
        if let Some((slot, ordinal)) = &after {
            predicates = predicates.and(
                refs.column(models::payload_refs::slot)
                    .gt(slot.as_str())?
                    .or(refs
                        .column(models::payload_refs::slot)
                        .eq(slot.as_str())?
                        .and(refs.column(models::payload_refs::ordinal).gt(*ordinal)?)),
            );
        }
        let page = db
            .from(&refs)
            .filter(predicates)
            .order_by(refs.column(models::payload_refs::slot).asc())
            .order_by(refs.column(models::payload_refs::ordinal).asc())
            .select(refs.row::<PayloadReference>())?
            .limit(page_limit)?
            .all()
            .await?;
        let count = page.len();
        let mut documents = Vec::with_capacity(count);
        for mut row in page {
            after = Some((row.slot.clone(), row.ordinal));
            row.id = super::super::types::storage_id();
            row.generation = target_generation;
            documents.push(Value::Object(row.into_record()?));
        }
        if !documents.is_empty() {
            db.collection(models::payload_refs::Entity::COLLECTION)?
                .execute(Operation::InsertMany {
                    documents: Value::Array(documents),
                })
                .await?;
        }
        if count < page_limit as usize {
            break;
        }
    }
    Ok(())
}
