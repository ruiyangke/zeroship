use super::{
    app::{
        emit, encode, lock_app, lock_app_state, lock_run, parse_state, request_result,
        store_request,
    },
    models,
    store::Transaction,
    types::digest,
    AppWorkflows, RequestId, WorkflowService,
};
use crate::{
    engine::StepCheckpoint,
    operations::{DeliveredSignal, SignalOptions},
    validation, WorkflowServiceError,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_data_orm::{
    orm::{Entity, FindOptions, FromRow, Operation, Output},
    sql::{CompareOp, Literal, Operand, Predicate},
    value,
};

#[derive(FromRow)]
#[orm(entity = models::broadcasts)]
struct PendingBroadcast {
    app_id: String,
    id: String,
}

#[derive(FromRow)]
#[orm(entity = models::broadcasts)]
struct BroadcastRecord {
    topic: String,
    signal_type: String,
    payload: String,
    created_at: i64,
    cursor: i64,
    cutoff_sequence: i64,
    origin: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedBroadcast {
    pub id: String,
}

pub(crate) fn validate_topic(topic: &str) -> Result<(), WorkflowServiceError> {
    if topic.is_empty() || topic.len() > 256 || topic.chars().any(char::is_control) {
        return Err(WorkflowServiceError::InvalidRequest(
            "invalid workflow topic".into(),
        ));
    }
    Ok(())
}

impl AppWorkflows {
    pub async fn broadcast(
        &self,
        request: &RequestId,
        topic: &str,
        options: SignalOptions,
    ) -> Result<AcceptedBroadcast, WorkflowServiceError> {
        let captured = self.capture_policy();
        captured
            .run(async {
                validate_topic(topic)?;
                validation::signal_type(&options.signal_type)?;
                let digest = digest(&(topic, &options))?;
                let mut tx = self.service.begin().await?;
                lock_app_state(&mut tx, &self.app).await?;
                let now = tx.now().await?;
                if let Some(receipt) =
                    request_result(&tx, &self.app, request, "broadcast", &digest).await?
                {
                    return Ok(receipt);
                }
                captured.check()?;
                let policy = &captured.authority()?.policy;
                if encode(&options.payload)?.len() > policy.max_input_bytes {
                    return Err(WorkflowServiceError::PayloadTooLarge);
                }
                let result = publish(&mut tx, &self.app, topic, &options, "app", now).await?;
                store_request(
                    &mut tx,
                    &self.app,
                    request,
                    "broadcast",
                    &digest,
                    &result,
                    now,
                )
                .await?;
                captured.check()?;
                tx.commit().await?;
                Ok(result)
            })
            .await
    }
}

pub(crate) async fn deliver(
    tx: &mut Transaction,
    app: &AppId,
    run_id: &str,
    options: &SignalOptions,
    origin: &str,
    now: i64,
) -> Result<DeliveredSignal, WorkflowServiceError> {
    let run = lock_run(tx, app, run_id).await?;
    if parse_state(&run.text("state")?)?.is_terminal() {
        return Err(WorkflowServiceError::Conflict(
            "cannot signal a terminal workflow run".into(),
        ));
    }
    let result = DeliveredSignal {
        id: typed_id::new_workflow_signal_id(),
    };
    tx.database().collection(models::signals::Entity::COLLECTION)?.insert(value!({
        "app_id":app.as_str(), "run_id":run_id, "id":result.id.clone(), "signal_type":options.signal_type.clone(),
        "payload":encode(&options.payload)?, "created_at":now, "origin":origin, "delivery":"direct",
    })).await?;
    if run.optional_text("task_id")?.is_none()
        && run.text("control")? == "none"
        && run.text("state")? == "waiting"
    {
        // The app and run locks keep the wait lookup and wake-up in the same frontier.
        let Output::Count(waiting) = tx.database().collection(models::waits::Entity::COLLECTION)?.count(
            value!({"app_id":app.as_str(), "run_id":run_id, "generation":run.integer("generation")?,
                "signal_type":options.signal_type.clone()}), value!({}),
        ).await? else {
            return Err(WorkflowServiceError::Internal("workflow count returned rows".into()));
        };
        if waiting != 0 {
            tx.database().collection(models::runs::Entity::COLLECTION)?.update(
                value!({"app_id":app.as_str(), "id":run_id, "generation":run.integer("generation")?,
                    "task_id":null, "control":"none", "state":"waiting"}),
                value!({"due_at":now}),
            ).await?;
            super::publication::advance(tx, app, run_id, now).await?;
        }
    }
    emit(
        tx,
        app,
        &result.id,
        "workflow.signal",
        json!({"runId":run_id}),
        now,
    )
    .await?;
    Ok(result)
}

pub(crate) async fn subscribe(
    tx: &mut Transaction,
    app: &AppId,
    run_id: &str,
    generation: i64,
    step: &StepCheckpoint,
    now: i64,
) -> Result<(), WorkflowServiceError> {
    let Some(topic) = &step.topic else {
        return Ok(());
    };
    validate_topic(topic)?;
    let sequence = subscription_sequence(tx, app)
        .await?
        .checked_add(1)
        .ok_or_else(|| {
            WorkflowServiceError::ResourceExhausted(
                "workflow subscription sequence exhausted".into(),
            )
        })?;
    tx.database()
        .collection(models::app_state::Entity::COLLECTION)?
        .update(
            value!({"app_id":app.as_str()}),
            value!({"subscription_sequence":sequence}),
        )
        .await?;
    tx.database().collection(models::subscriptions::Entity::COLLECTION)?.insert(value!({
        "app_id":app.as_str(), "run_id":run_id, "generation":generation, "ordinal":i64::from(step.ordinal),
        "id":typed_id::new_workflow_subscription_id(), "topic":topic.as_str(), "created_at":now, "sequence":sequence,
    })).await?;
    Ok(())
}

impl WorkflowService {
    /// Resume durable fanout from its recipient cursor. A publication includes
    /// subscriptions accepted before it, even when the host restarts mid-fanout.
    pub async fn tick_broadcasts(&self) -> Result<usize, WorkflowServiceError> {
        let tx = self.begin().await?;
        let db = tx.database();
        let broadcast = db.entity::<models::broadcasts::Entity>()?.alias("b")?;
        let pending = db
            .from(&broadcast)
            .filter(Predicate::And(vec![
                Predicate::Or(
                    tx.host_app_ids()?
                        .into_iter()
                        .map(|app| {
                            broadcast
                                .column(models::broadcasts::app_id)
                                .eq(app.as_str())
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                ),
                broadcast.column(models::broadcasts::finished).eq(0_i64)?,
            ]))
            .order_by(broadcast.column(models::broadcasts::created_at).asc())
            .order_by(broadcast.column(models::broadcasts::app_id).asc())
            .order_by(broadcast.column(models::broadcasts::id).asc())
            .select(broadcast.row::<PendingBroadcast>())?
            .limit(128)?
            .all()
            .await?;
        tx.commit().await?;
        let mut delivered = 0;
        for candidate in pending {
            let app = AppId::parse(&candidate.app_id).map_err(|_| {
                WorkflowServiceError::Internal("invalid persisted workflow app identity".into())
            })?;
            let id = candidate.id;
            let mut tx = self.begin().await?;
            lock_app(&mut tx, &app).await?;
            let now = tx.now().await?;
            // Every fanout mutation holds the app lock, including publication.
            // Read the receipt after acquiring it so concurrent ticks serialize.
            let rows = tx
                .database()
                .entity::<models::broadcasts::Entity>()?
                .find::<BroadcastRecord>(
                    models::broadcasts::app_id
                        .eq(app.as_str())?
                        .and(models::broadcasts::id.eq(id.as_str())?)
                        .and(models::broadcasts::finished.eq(0_i64)?),
                    FindOptions {
                        limit: Some(1),
                        ..Default::default()
                    },
                )
                .await?;
            let Some(broadcast) = rows.first() else {
                tx.commit().await?;
                continue;
            };
            let db = tx.database();
            let subscription = db.entity::<models::subscriptions::Entity>()?.alias("s")?;
            let run = db.entity::<models::runs::Entity>()?.alias("r")?;
            let wait = db.entity::<models::waits::Entity>()?.alias("w")?;
            let recipients = db
                .from(&subscription)
                .inner_join(
                    &run,
                    Predicate::And(vec![
                        subscription
                            .column(models::subscriptions::app_id)
                            .eq_column(run.column(models::runs::app_id))?,
                        subscription
                            .column(models::subscriptions::run_id)
                            .eq_column(run.column(models::runs::id))?,
                        subscription
                            .column(models::subscriptions::generation)
                            .eq_column(run.column(models::runs::generation))?,
                    ]),
                )?
                .inner_join(
                    &wait,
                    Predicate::And(vec![
                        subscription
                            .column(models::subscriptions::app_id)
                            .eq_column(wait.column(models::waits::app_id))?,
                        subscription
                            .column(models::subscriptions::run_id)
                            .eq_column(wait.column(models::waits::run_id))?,
                        subscription
                            .column(models::subscriptions::generation)
                            .eq_column(wait.column(models::waits::generation))?,
                        subscription
                            .column(models::subscriptions::ordinal)
                            .eq_column(wait.column(models::waits::ordinal))?,
                    ]),
                )?
                .filter(Predicate::And(vec![
                    subscription
                        .column(models::subscriptions::app_id)
                        .eq(app.as_str())?,
                    subscription
                        .column(models::subscriptions::topic)
                        .eq(broadcast.topic.as_str())?,
                    Predicate::compare(
                        Operand::Path(
                            subscription
                                .column(models::subscriptions::sequence)
                                .asc()
                                .path,
                        ),
                        CompareOp::Gt,
                        Operand::Lit(Literal::Int(broadcast.cursor)),
                    ),
                    Predicate::compare(
                        Operand::Path(
                            subscription
                                .column(models::subscriptions::sequence)
                                .asc()
                                .path,
                        ),
                        CompareOp::Lte,
                        Operand::Lit(Literal::Int(broadcast.cutoff_sequence)),
                    ),
                    wait.column(models::waits::signal_type)
                        .eq(Some(broadcast.signal_type.as_str()))?,
                    Predicate::Not(Box::new(Predicate::Or(vec![
                        run.column(models::runs::state).eq("completed")?,
                        run.column(models::runs::state).eq("failed")?,
                        run.column(models::runs::state).eq("cancelled")?,
                    ]))),
                ]))
                .order_by(subscription.column(models::subscriptions::sequence).asc())
                .select(subscription.row::<models::SubscriptionRecipient>())?
                .limit(128)?
                .all()
                .await?;
            let signals = db.collection(models::signals::Entity::COLLECTION)?;
            let mut cursor = broadcast.cursor;
            for recipient in &recipients {
                cursor = recipient.sequence;
                let Output::Count(existing) = signals
                    .count(
                        value!({"app_id":app.as_str(), "broadcast_id":id.clone(), "run_id":recipient.run_id.clone()}),
                        value!({}),
                    )
                    .await?
                else {
                    return Err(WorkflowServiceError::Internal(
                        "workflow signal count returned rows".into(),
                    ));
                };
                if existing != 0 {
                    continue;
                }
                let signal_id = typed_id::new_workflow_signal_id();
                signals.insert(value!({
                    "app_id":app.as_str(), "id":signal_id.clone(), "run_id":recipient.run_id.clone(),
                    "signal_type":broadcast.signal_type.clone(), "payload":broadcast.payload.clone(),
                    "created_at":broadcast.created_at, "broadcast_id":id.clone(), "origin":broadcast.origin.clone(),
                    "delivery":"topic", "topic":broadcast.topic.clone(), "target_generation":recipient.generation,
                    "target_ordinal":recipient.ordinal,
                })).await?;
                delivered += 1;
                let woke = tx.database().collection(models::runs::Entity::COLLECTION)?.execute(Operation::Update {
                    filter:
                    value!({"app_id":app.as_str(), "id":recipient.run_id.clone(), "generation":recipient.generation,
                        "task_id":null, "control":"none", "state":"waiting"}), patch:value!({"due_at":now}), many:true,
                }).await?;
                if matches!(woke, Output::Count(1)) {
                    super::publication::advance(&tx, &app, &recipient.run_id, now).await?;
                }
                emit(
                    &mut tx,
                    &app,
                    &signal_id,
                    "workflow.signal",
                    json!({"runId":recipient.run_id,"broadcastId":id}),
                    now,
                )
                .await?;
            }
            tx.database()
                .collection(models::broadcasts::Entity::COLLECTION)?
                .update(
                    value!({"app_id":app.as_str(), "id":id}),
                    value!({"cursor":cursor, "finished":i64::from(recipients.len() < 128)}),
                )
                .await?;
            tx.commit().await?;
        }
        Ok(delivered)
    }
}

pub(crate) async fn publish(
    tx: &mut Transaction,
    app: &AppId,
    topic: &str,
    options: &SignalOptions,
    origin: &str,
    now: i64,
) -> Result<AcceptedBroadcast, WorkflowServiceError> {
    let cutoff = subscription_sequence(tx, app).await?;
    let result = AcceptedBroadcast {
        id: typed_id::new_workflow_broadcast_id(),
    };
    tx.database().collection(models::broadcasts::Entity::COLLECTION)?.insert(value!({
        "app_id":app.as_str(), "id":result.id.clone(), "topic":topic, "signal_type":options.signal_type.clone(),
        "payload":encode(&options.payload)?, "created_at":now, "cursor":0, "cutoff_sequence":cutoff,
        "origin":origin, "finished":0,
    })).await?;
    emit(
        tx,
        app,
        &result.id,
        "workflow.broadcast",
        json!({"topic":topic}),
        now,
    )
    .await?;
    Ok(result)
}

async fn subscription_sequence(tx: &Transaction, app: &AppId) -> Result<i64, WorkflowServiceError> {
    Ok(tx
        .database()
        .entity::<models::app_state::Entity>()?
        .find::<models::SubscriptionSequence>(
            models::app_state::app_id.eq(app.as_str())?,
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| WorkflowServiceError::Internal("workflow app state is missing".into()))?
        .subscription_sequence)
}
