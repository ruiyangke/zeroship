use super::{
    changed_once, increment, invalid, models, AppId, Broadcast, Transaction, WorkflowServiceError,
};
use std::collections::{BTreeMap, BTreeSet};
use zeroship_data_orm::{
    orm::{FindOptions, FromRow, Insertable},
    sql::MAX_MEMBERSHIP_LIST_LEN,
};

#[derive(FromRow)]
#[orm(entity = models::app_state)]
struct Counter {
    signal_sequence: i64,
}

/// Reserve the app's next delivery sequence.
pub(in crate::service) async fn allocate(
    tx: &Transaction,
    app: &AppId,
) -> Result<i64, WorkflowServiceError> {
    Ok(*allocate_block(tx, app, 1).await?.start())
}

/// Reserve `count` consecutive delivery sequences in one counter write,
/// returning them. The last may be `i64::MAX`.
async fn allocate_block(
    tx: &Transaction,
    app: &AppId,
    count: usize,
) -> Result<std::ops::RangeInclusive<i64>, WorkflowServiceError> {
    let current = counter(tx, app).await?;
    let first = increment(current, "workflow signal delivery sequence exhausted")?;
    let last = i64::try_from(count)
        .ok()
        .filter(|count| *count > 0)
        .ok_or_else(invalid)?
        .checked_add(current)
        .ok_or_else(|| {
            WorkflowServiceError::ResourceExhausted(
                "workflow signal delivery sequence exhausted".into(),
            )
        })?;
    changed_once(
        tx.database()
            .entity::<models::app_state::Entity>()?
            .update_many(
                models::app_state::app_id
                    .eq(app.as_str())?
                    .and(models::app_state::signal_sequence.eq(current)?),
                models::app_state::signal_sequence.set(last)?,
            )
            .await?,
        invalid,
    )?;
    Ok(first..=last)
}

async fn counter(tx: &Transaction, app: &AppId) -> Result<i64, WorkflowServiceError> {
    let row = tx
        .database()
        .entity::<models::app_state::Entity>()?
        .find::<Counter>(models::app_state::app_id.eq(app.as_str())?, super::one())
        .await?
        .into_iter()
        .next()
        .ok_or_else(invalid)?;
    if row.signal_sequence < 0 {
        return Err(invalid());
    }
    Ok(row.signal_sequence)
}

pub(in crate::service) async fn validate_sequence(
    tx: &Transaction,
    app: &AppId,
    value: i64,
) -> Result<(), WorkflowServiceError> {
    if value <= 0 || value > counter(tx, app).await? {
        return Err(invalid());
    }
    Ok(())
}

#[derive(FromRow, Insertable)]
#[orm(entity = models::signals)]
pub(in crate::service) struct SignalRecord {
    pub id: String,
    pub app_id: String,
    pub run_id: String,
    pub signal_type: String,
    pub payload: String,
    pub created_at: i64,
    pub delivery_sequence: i64,
    pub origin: String,
    pub delivery: String,
    pub broadcast_id: Option<String>,
    pub topic: Option<String>,
    pub target_generation: Option<i64>,
    pub target_ordinal: Option<i64>,
}

#[derive(FromRow)]
#[orm(entity = models::signals)]
struct Delivered {
    run_id: String,
}

/// Materialize this broadcast's signal for every recipient run of the page
/// that does not hold one yet, returning those recipients in page order.
///
/// A run already holding the broadcast's signal, from an earlier page or an
/// earlier recipient row of this one, receives no second. Each statement covers
/// the whole page, or for a membership read at most [`MAX_MEMBERSHIP_LIST_LEN`]
/// runs of it, so the round trips grow by one per membership list instead of
/// one per recipient.
pub(super) async fn materialize<'a>(
    tx: &Transaction,
    app: &AppId,
    broadcast: &Broadcast,
    recipients: &'a [models::SubscriptionRecipient],
    now: i64,
) -> Result<Vec<&'a models::SubscriptionRecipient>, WorkflowServiceError> {
    let delivered = delivered(tx, app, broadcast, recipients).await?;
    let mut named = BTreeSet::new();
    let fresh: Vec<_> = recipients
        .iter()
        .filter(|recipient| {
            !delivered.contains(recipient.run_id.as_str())
                && named.insert(recipient.run_id.as_str())
        })
        .collect();
    if fresh.is_empty() {
        return Ok(fresh);
    }
    let sequences = allocate_block(tx, app, fresh.len()).await?;
    let records: Vec<SignalRecord> = fresh
        .iter()
        .zip(sequences)
        .map(|(recipient, sequence)| SignalRecord {
            id: zeroship_core::typed_id::new_workflow_signal_id(),
            app_id: app.as_str().to_owned(),
            run_id: recipient.run_id.clone(),
            signal_type: broadcast.signal_type.clone(),
            payload: broadcast.payload.clone(),
            created_at: broadcast.created_at,
            delivery_sequence: sequence,
            origin: broadcast.origin.clone(),
            delivery: "topic".into(),
            broadcast_id: Some(broadcast.id.clone()),
            topic: Some(broadcast.topic.clone()),
            target_generation: Some(recipient.generation),
            target_ordinal: Some(recipient.ordinal),
        })
        .collect();
    let expected: BTreeMap<String, i64> = records
        .iter()
        .map(|record| (record.id.clone(), record.delivery_sequence))
        .collect();
    let events: Vec<(String, serde_json::Value)> = records
        .iter()
        .map(|record| {
            (
                record.id.clone(),
                serde_json::json!({"runId":record.run_id,"broadcastId":broadcast.id}),
            )
        })
        .collect();
    let saved: Vec<SignalRecord> =
        Box::pin(tx.insert_rows::<models::signals::Entity, _, _>(records)).await?;
    if saved.len() != expected.len()
        || saved
            .iter()
            .any(|row| expected.get(&row.id) != Some(&row.delivery_sequence))
    {
        return Err(invalid());
    }
    Box::pin(super::super::app::emit_all(
        tx,
        app,
        "workflow.signal",
        events,
        now,
    ))
    .await?;
    Ok(fresh)
}

/// The recipient runs that already hold this broadcast's signal. The journal
/// keeps at most one per run, so a membership list of runs reads at most as
/// many rows.
async fn delivered(
    tx: &Transaction,
    app: &AppId,
    broadcast: &Broadcast,
    recipients: &[models::SubscriptionRecipient],
) -> Result<BTreeSet<String>, WorkflowServiceError> {
    let runs: Vec<&str> = recipients
        .iter()
        .map(|recipient| recipient.run_id.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut delivered = BTreeSet::new();
    for chunk in runs.chunks(MAX_MEMBERSHIP_LIST_LEN) {
        let rows = tx
            .database()
            .entity::<models::signals::Entity>()?
            .find::<Delivered>(
                models::signals::app_id
                    .eq(app.as_str())?
                    .and(models::signals::broadcast_id.eq(Some(broadcast.id.as_str()))?)
                    .and(models::signals::run_id.in_values(chunk.iter().copied())?),
                FindOptions {
                    limit: Some(i64::try_from(chunk.len()).map_err(|_| invalid())?),
                    ..Default::default()
                },
            )
            .await?;
        delivered.extend(rows.into_iter().map(|row| row.run_id));
    }
    Ok(delivered)
}
