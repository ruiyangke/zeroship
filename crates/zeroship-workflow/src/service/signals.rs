use super::{
    app::{
        emit, encode, lock_app_state, lock_run, parse_state, request_result, require_open_epoch,
        store_request,
    },
    models,
    policy::CapturedPolicy,
    store::Transaction,
    types::digest,
    AppWorkflows, RequestId,
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
    /// Accept a topic broadcast. Its fanout pages become publication intents,
    /// so acceptance is fenced by the ingress epoch like any other ingress.
    ///
    /// # Errors
    /// Rejects invalid topics and signals, oversized payloads, reused request
    /// identities, unavailable authority, a closed ingress epoch and journal
    /// failures.
    pub async fn broadcast(
        &self,
        request: &RequestId,
        topic: &str,
        options: SignalOptions,
    ) -> Result<AcceptedBroadcast, WorkflowServiceError> {
        self.accept(|scope| {
            let options = options.clone();
            Box::pin(async move {
                let captured = scope.capture_policy();
                captured
                    .run(scope.broadcast_captured(request, topic, &options, &captured))
                    .await
            })
        })
        .await
    }

    async fn broadcast_captured(
        &self,
        request: &RequestId,
        topic: &str,
        options: &SignalOptions,
        captured: &CapturedPolicy,
    ) -> Result<AcceptedBroadcast, WorkflowServiceError> {
        validate_topic(topic)?;
        validation::signal_type(&options.signal_type)?;
        let digest = digest(&(topic, options))?;
        let mut tx = self.service.begin().await?;
        lock_app_state(&mut tx, &self.app).await?;
        let now = tx.now().await?;
        if let Some(receipt) = request_result(&tx, &self.app, request, "broadcast", &digest).await?
        {
            return Ok(receipt);
        }
        captured.check()?;
        require_open_epoch(&tx, &self.app, captured).await?;
        let policy = &captured.authority()?.policy;
        if encode(&options.payload)?.len() > policy.max_input_bytes {
            return Err(WorkflowServiceError::PayloadTooLarge);
        }
        let result = publish(&mut tx, &self.app, topic, options, "app", now).await?;
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
    let sequence = super::fanout::signals::allocate(tx, app).await?;
    let saved = tx
        .database()
        .entity::<models::signals::Entity>()?
        .insert::<_, super::fanout::signals::SignalRecord>(super::fanout::signals::SignalRecord {
            app_id: app.as_str().to_owned(),
            run_id: run_id.to_owned(),
            id: result.id.clone(),
            signal_type: options.signal_type.clone(),
            payload: encode(&options.payload)?,
            created_at: now,
            delivery_sequence: sequence,
            origin: origin.to_owned(),
            delivery: "direct".into(),
            broadcast_id: None,
            topic: None,
            target_generation: None,
            target_ordinal: None,
        })
        .await?;
    if saved.id != result.id || saved.delivery_sequence != sequence {
        return Err(WorkflowServiceError::Internal(
            "invalid persisted workflow signal".into(),
        ));
    }

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

pub(crate) async fn publish(
    tx: &mut Transaction,
    app: &AppId,
    topic: &str,
    options: &SignalOptions,
    origin: &str,
    now: i64,
) -> Result<AcceptedBroadcast, WorkflowServiceError> {
    let cutoff = subscription_sequence(tx, app).await?;
    let result = Box::pin(super::fanout::accept(
        tx, app, topic, options, origin, cutoff, now,
    ))
    .await?;
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
