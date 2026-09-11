use super::{
    app::{deadline, emit, encode, lock_app, lock_run, parse_state, request_result, store_request},
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
        let mut tx = self.service.store.begin().await?;
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
        let apps = tx.table("apps");
        let rows = tx
            .query(
                &format!("SELECT subscription_sequence FROM {apps} WHERE app_id=$1"),
                &[self.app.as_str().into()],
            )
            .await?;
        let cutoff = rows[0].integer("subscription_sequence")?;
        let broadcasts = tx.table("broadcasts");
        let result = AcceptedBroadcast {
            id: typed_id::new_workflow_broadcast_id(),
        };
        tx.execute(&format!("INSERT INTO {broadcasts} (app_id,id,topic,signal_type,payload,created_at,cursor,cutoff_sequence,origin,finished) VALUES ($1,$2,$3,$4,$5,$6,0,$7,'app',0)"),
            &[self.app.as_str().into(),result.id.clone().into(),topic.into(),options.signal_type.into(),encode(&options.payload)?.into(),now.into(),cutoff.into()]).await?;
        emit(
            &mut tx,
            &self.app,
            &result.id,
            "workflow.broadcast",
            json!({"topic":topic}),
            now,
        )
        .await?;
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
    let signals = tx.table("signals");
    let result = DeliveredSignal {
        id: typed_id::new_workflow_signal_id(),
    };
    tx.execute(&format!("INSERT INTO {signals} (app_id,run_id,id,signal_type,payload,created_at,origin,delivery) VALUES ($1,$2,$3,$4,$5,$6,$7,'direct')"), &[app.as_str().into(),run_id.into(),result.id.clone().into(),options.signal_type.clone().into(),encode(&options.payload)?.into(),now.into(),origin.into()]).await?;
    let runs = tx.table("runs");
    let waits = tx.table("waits");
    tx.execute(&format!("UPDATE {runs} SET due_at=$3 WHERE app_id=$1 AND id=$2 AND task_id IS NULL AND control='none' AND state='waiting' AND EXISTS (SELECT 1 FROM {waits} w WHERE w.app_id=$1 AND w.run_id=$2 AND w.generation={runs}.generation AND w.signal_type=$4)"), &[app.as_str().into(),run_id.into(),now.into(),options.signal_type.clone().into()]).await?;
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
    let apps = tx.table("apps");
    let rows = tx
        .query(
            &format!("SELECT subscription_sequence FROM {apps} WHERE app_id=$1"),
            &[app.as_str().into()],
        )
        .await?;
    let sequence = rows[0]
        .integer("subscription_sequence")?
        .checked_add(1)
        .ok_or_else(|| {
            WorkflowServiceError::ResourceExhausted(
                "workflow subscription sequence exhausted".into(),
            )
        })?;
    tx.execute(
        &format!("UPDATE {apps} SET subscription_sequence=$2 WHERE app_id=$1"),
        &[app.as_str().into(), sequence.into()],
    )
    .await?;
    let subscriptions = tx.table("subscriptions");
    tx.execute(&format!("INSERT INTO {subscriptions} (app_id,run_id,generation,ordinal,id,topic,created_at,sequence) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)"), &[app.as_str().into(),run_id.into(),generation.into(),i64::from(step.ordinal).into(),typed_id::new_workflow_subscription_id().into(),topic.clone().into(),now.into(),sequence.into()]).await?;
    Ok(())
}

impl WorkflowService {
    /// Resume durable fanout from its recipient cursor. A publication includes
    /// subscriptions accepted before it, even when the host restarts mid-fanout.
    pub async fn tick_broadcasts(&self) -> Result<usize, WorkflowServiceError> {
        let mut tx = self.store.begin().await?;
        let broadcasts = tx.table("broadcasts");
        let pending=tx.query(&format!("SELECT app_id,id FROM {broadcasts} WHERE finished=0 ORDER BY created_at,app_id,id LIMIT 128"), &[]).await?;
        tx.commit().await?;
        let mut delivered = 0;
        for candidate in pending {
            let app = AppId::parse(&candidate.text("app_id")?).map_err(|_| {
                WorkflowServiceError::Internal("invalid persisted workflow app identity".into())
            })?;
            let id = candidate.text("id")?;
            let mut tx = self.store.begin().await?;
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
            let subscriptions = tx.table("subscriptions");
            let runs = tx.table("runs");
            let waits = tx.table("waits");
            let recipients=tx.query(&format!("SELECT s.run_id,s.generation,s.ordinal,s.sequence FROM {subscriptions} s JOIN {runs} r ON r.app_id=s.app_id AND r.id=s.run_id AND r.generation=s.generation JOIN {waits} w ON w.app_id=s.app_id AND w.run_id=s.run_id AND w.generation=s.generation AND w.ordinal=s.ordinal WHERE s.app_id=$1 AND s.topic=$2 AND s.sequence>$3 AND s.sequence<=$4 AND r.state NOT IN ('completed','failed','cancelled') AND w.signal_type=$5 ORDER BY s.sequence LIMIT 128"),
                &[app.as_str().into(),broadcast.text("topic")?.into(),broadcast.integer("cursor")?.into(),broadcast.integer("cutoff_sequence")?.into(),broadcast.text("signal_type")?.into()]).await?;
            let signals = tx.table("signals");
            let mut cursor = broadcast.integer("cursor")?;
            for recipient in &recipients {
                cursor = recipient.integer("sequence")?;
                let signal_id = typed_id::new_workflow_signal_id();
                let inserted=tx.execute(&format!("INSERT INTO {signals} (app_id,id,run_id,signal_type,payload,created_at,broadcast_id,origin,delivery,topic,target_generation,target_ordinal) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'topic',$9,$10,$11) ON CONFLICT (app_id,broadcast_id,run_id) DO NOTHING"),
                    &[app.as_str().into(),signal_id.clone().into(),recipient.text("run_id")?.into(),broadcast.text("signal_type")?.into(),broadcast.text("payload")?.into(),broadcast.integer("created_at")?.into(),id.clone().into(),broadcast.text("origin")?.into(),broadcast.text("topic")?.into(),recipient.integer("generation")?.into(),recipient.integer("ordinal")?.into()]).await?;
                if inserted == 0 {
                    continue;
                }
                delivered += 1;
                tx.execute(&format!("UPDATE {runs} SET due_at=$4 WHERE app_id=$1 AND id=$2 AND generation=$3 AND task_id IS NULL AND control='none' AND state='waiting'"), &[app.as_str().into(),recipient.text("run_id")?.into(),recipient.integer("generation")?.into(),now.into()]).await?;
                emit(
                    &mut tx,
                    &app,
                    &signal_id,
                    "workflow.signal",
                    json!({"runId":recipient.text("run_id")?,"broadcastId":id}),
                    now,
                )
                .await?;
            }
            tx.execute(
                &format!("UPDATE {broadcasts} SET cursor=$3,finished=$4 WHERE app_id=$1 AND id=$2"),
                &[
                    app.as_str().into(),
                    id.into(),
                    cursor.into(),
                    i64::from(recipients.len() < 128).into(),
                ],
            )
            .await?;
            tx.commit().await?;
        }
        Ok(delivered)
    }
}
