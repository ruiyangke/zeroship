use super::{
    changed_once, increment, invalid, models, one, AppId, Broadcast, Transaction,
    WorkflowServiceError,
};
use zeroship_data_orm::orm::{FromRow, Insertable};

#[derive(FromRow)]
#[orm(entity = models::app_state)]
struct Counter {
    signal_sequence: i64,
}

pub(in crate::service) async fn allocate(
    tx: &Transaction,
    app: &AppId,
) -> Result<i64, WorkflowServiceError> {
    let current = counter(tx, app).await?;
    let next = increment(current, "workflow signal delivery sequence exhausted")?;
    changed_once(
        tx.database()
            .entity::<models::app_state::Entity>()?
            .update_many(
                models::app_state::app_id
                    .eq(app.as_str())?
                    .and(models::app_state::signal_sequence.eq(current)?),
                models::app_state::signal_sequence.set(next)?,
            )
            .await?, invalid
    )?;
    Ok(next)
}

async fn counter(tx: &Transaction, app: &AppId) -> Result<i64, WorkflowServiceError> {
    let row = tx
        .database()
        .entity::<models::app_state::Entity>()?
        .find::<Counter>(models::app_state::app_id.eq(app.as_str())?, one())
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

pub(super) async fn materialize(
    tx: &mut Transaction,
    app: &AppId,
    broadcast: &Broadcast,
    recipient: &models::SubscriptionRecipient,
    now: i64,
) -> Result<bool, WorkflowServiceError> {
    if tx
        .database()
        .entity::<models::signals::Entity>()?
        .exists(
            models::signals::app_id
                .eq(app.as_str())?
                .and(models::signals::broadcast_id.eq(Some(broadcast.id.as_str()))?)
                .and(models::signals::run_id.eq(recipient.run_id.as_str())?),
        )
        .await?
    {
        return Ok(false);
    }
    let sequence = allocate(tx, app).await?;
    let id = zeroship_core::typed_id::new_workflow_signal_id();
    let saved = tx
        .database()
        .entity::<models::signals::Entity>()?
        .insert::<_, SignalRecord>(SignalRecord {
            id: id.clone(),
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
        .await?;
    if saved.id != id || saved.delivery_sequence != sequence {
        return Err(invalid());
    }
    super::super::app::emit(
        tx,
        app,
        &id,
        "workflow.signal",
        serde_json::json!({"runId":recipient.run_id,"broadcastId":broadcast.id}),
        now,
    )
    .await?;
    Ok(true)
}
