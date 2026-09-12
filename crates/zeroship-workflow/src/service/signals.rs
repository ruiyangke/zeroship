use super::{
    app::{deadline, emit, encode, lock_app, lock_run, parse_state, request_result, store_request},
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
    orm::{Entity, FindOptions, Output},
    sql::Predicate,
    value,
};

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
        validate_topic(topic)?;
        validation::signal_type(&options.signal_type)?;
        let digest = digest(&(topic, &options))?;
        let mut tx = self.service.begin().await?;
        let policy = lock_app(&mut tx, &self.app).await?;
        let now = tx.now().await?;
        if let Some(receipt) =
            request_result(&mut tx, &self.app, request, "broadcast", &digest, now).await?
        {
            return Ok(receipt);
        }
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
            deadline(now, policy.request_retention_ms)?,
        )
        .await?;
        tx.commit().await?;
        Ok(result)
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
        let mut tx = self.begin().await?;
        let broadcasts = tx.table("broadcasts");
        let (scope, app_ids) = tx.host_app_scope()?;
        let pending=tx.query(&format!("SELECT app_id,id FROM {broadcasts} WHERE app_id IN ({scope}) AND finished=0 ORDER BY created_at,app_id,id LIMIT 128"), &[app_ids]).await?;
        tx.commit().await?;
        let mut delivered = 0;
        for candidate in pending {
            let app = AppId::parse(&candidate.text("app_id")?).map_err(|_| {
                WorkflowServiceError::Internal("invalid persisted workflow app identity".into())
            })?;
            let id = candidate.text("id")?;
            let mut tx = self.begin().await?;
            lock_app(&mut tx, &app).await?;
            let now = tx.now().await?;
            let rows = tx
                .query(
                    &format!(
                        "SELECT * FROM {broadcasts} WHERE app_id=$1 AND id=$2 AND finished=0{}",
                        tx.lock_clause()
                    ),
                    &[app.as_str().into(), id.clone().into()],
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
                        .eq(broadcast.text("topic")?)?,
                    subscription
                        .column(models::subscriptions::sequence)
                        .gt(broadcast.integer("cursor")?)?,
                    subscription
                        .column(models::subscriptions::sequence)
                        .lte(broadcast.integer("cutoff_sequence")?)?,
                    wait.column(models::waits::signal_type)
                        .eq(Some(broadcast.text("signal_type")?))?,
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
            let signals = tx.table("signals");
            let mut cursor = broadcast.integer("cursor")?;
            for recipient in &recipients {
                cursor = recipient.sequence;
                let signal_id = typed_id::new_workflow_signal_id();
                let inserted=tx.execute(&format!("INSERT INTO {signals} (app_id,id,run_id,signal_type,payload,created_at,broadcast_id,origin,delivery,topic,target_generation,target_ordinal) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'topic',$9,$10,$11) ON CONFLICT (app_id,broadcast_id,run_id) DO NOTHING"),
                    &[app.as_str().into(),signal_id.clone().into(),recipient.run_id.clone().into(),broadcast.text("signal_type")?.into(),broadcast.text("payload")?.into(),broadcast.integer("created_at")?.into(),id.clone().into(),broadcast.text("origin")?.into(),broadcast.text("topic")?.into(),recipient.generation.into(),recipient.ordinal.into()]).await?;
                if inserted == 0 {
                    continue;
                }
                delivered += 1;
                tx.database().collection(models::runs::Entity::COLLECTION)?.update(
                    value!({"app_id":app.as_str(), "id":recipient.run_id.clone(), "generation":recipient.generation,
                        "task_id":null, "control":"none", "state":"waiting"}), value!({"due_at":now}),
                ).await?;
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
