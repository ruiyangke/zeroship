//! Creator-owned bounded materialization of an already accepted topic broadcast.

#![expect(
    clippy::future_not_send,
    reason = "fanout owns a compio-local creator transaction"
)]

use super::{
    app::{decode, encode, lock_app_state},
    delivery::{self, CapturedLease, JobReceipt},
    models, publication,
    store::Transaction,
    AcceptedBroadcast, AppWorkflows,
};
use crate::{operations::SignalOptions, WorkflowServiceError};
use zeroship_core::{
    app_id::AppId,
    workflow_jobs::{BroadcastId, JobOperation, JobOutcome, JobSpec},
};
use zeroship_data_orm::orm::FindOptions;

mod application;
mod history;
mod recipients;
mod records;
pub(super) mod signals;
use records::{Broadcast, Page, PageResult, Pending, TopicRecord};

/// Upper bound on eligible subscription work committed by one delivered page.
#[derive(Debug, Clone, Copy)]
pub struct FanoutOptions {
    pub page_size: u32,
}
impl Default for FanoutOptions {
    fn default() -> Self {
        Self { page_size: 128 }
    }
}
impl FanoutOptions {
    pub(super) fn validate(self) -> Result<(), WorkflowServiceError> {
        if self.page_size == 0 || self.page_size > 1024 {
            return Err(WorkflowServiceError::InvalidRequest(
                "invalid workflow fanout page bound".into(),
            ));
        }
        Ok(())
    }
}

impl AppWorkflows {
    /// Materialize one topic page. A later broadcast remains unacknowledged until
    /// its predecessor finishes. Committed pages replay without fresh authority.
    ///
    /// # Errors
    /// Refuses changed jobs, invalid journal linkage and exhausted original policy
    /// or delivery authority. Failed transactions preserve the same retryable page.
    pub async fn fanout_job(
        &self,
        grant: &impl zeroship_core::workflow_jobs::JobLease,
        options: FanoutOptions,
    ) -> Result<Option<JobReceipt>, WorkflowServiceError> {
        options.validate()?;
        let job = &grant.delivery().job;
        delivery::check_scope(self.app_id(), job)?;
        if !matches!(job.operation, JobOperation::Fanout { .. }) {
            return Err(WorkflowServiceError::InvalidRequest(
                "expected workflow fanout job".into(),
            ));
        }
        let captured = CapturedLease::capture(self, grant)
            .and_then(|authority| Ok((authority.bind(self)?, authority)));
        let policy = captured
            .as_ref()
            .ok()
            .map(|(scope, _)| scope.capture_policy());
        let budget =
            delivery::attempt_budget(captured.as_ref().ok().map(|(_, authority)| authority), None);
        let operation = delivery::run_attempt(
            None,
            budget,
            Box::pin(async {
                let scope = captured.as_ref().map_or(self, |(scope, _)| scope);
                let mut tx = if captured.is_ok() {
                    scope.service.begin().await?
                } else {
                    scope.service.begin_history().await?
                };
                lock_app_state(&mut tx, self.app_id()).await?;
                if let Some(receipt) = history::receipt(&tx, job).await? {
                    tx.commit().await?;
                    return Ok(Some(receipt));
                }
                let (scope, authority) = captured?;
                authority.check(&scope)?;
                tx.capture_mutation(self.app_id())?;
                let result = Box::pin(application::apply(&mut tx, job, options)).await?;
                authority.check(&scope)?;
                tx.commit().await?;
                authority.check(&scope)?;
                Ok(result)
            }),
        );
        match policy {
            Some(policy) => policy.run(operation).await,
            None => operation.await,
        }
    }
}

pub(super) async fn receipt(
    tx: &Transaction,
    job: &JobSpec,
) -> Result<Option<JobReceipt>, WorkflowServiceError> {
    history::receipt(tx, job).await
}

pub(super) async fn accept(
    tx: &Transaction,
    app: &AppId,
    topic: &str,
    options: &SignalOptions,
    origin: &str,
    cutoff: i64,
    now: i64,
) -> Result<AcceptedBroadcast, WorkflowServiceError> {
    let topic_row = match topic_record(tx, app, topic).await? {
        Some(topic) => topic,
        None => {
            tx.database()
                .entity::<models::topics::Entity>()?
                .insert::<_, TopicRecord>(TopicRecord {
                    id: super::types::storage_id(),
                    app_id: app.as_str().to_owned(),
                    topic: topic.to_owned(),
                    signal_epoch: 0,
                    accepted_sequence: 0,
                    completed_sequence: 0,
                })
                .await?
        }
    };
    topic_row.validate(app, topic)?;
    let sequence = increment(
        topic_row.accepted_sequence,
        "workflow broadcast sequence exhausted",
    )?;
    changed(
        tx.database()
            .entity::<models::topics::Entity>()?
            .update_many(
                models::topics::id
                    .eq(topic_row.id.as_str())?
                    .and(models::topics::accepted_sequence.eq(topic_row.accepted_sequence)?),
                models::topics::accepted_sequence.set(sequence)?,
            )
            .await?,
    )?;
    let id = BroadcastId::mint();
    let saved = tx
        .database()
        .entity::<models::broadcasts::Entity>()?
        .insert::<_, Broadcast>(Broadcast {
            id: id.as_str().to_owned(),
            app_id: app.as_str().to_owned(),
            topic: topic.to_owned(),
            signal_type: options.signal_type.clone(),
            payload: encode(&options.payload)?,
            created_at: now,
            cursor: 0,
            cutoff_sequence: cutoff,
            origin: origin.to_owned(),
            finished: 0,
            sequence,
            revision: 1,
        })
        .await?;
    saved.validate(app, &id)?;
    publication::fanout(tx, app, &id, 1.try_into().map_err(|_| invalid())?, now).await?;
    Ok(AcceptedBroadcast {
        id: id.as_str().to_owned(),
    })
}

async fn topic_record(
    tx: &Transaction,
    app: &AppId,
    topic: &str,
) -> Result<Option<TopicRecord>, WorkflowServiceError> {
    let row = tx
        .database()
        .entity::<models::topics::Entity>()?
        .find::<TopicRecord>(
            models::topics::app_id
                .eq(app.as_str())?
                .and(models::topics::topic.eq(topic)?),
            one(),
        )
        .await?
        .into_iter()
        .next();
    if let Some(row) = &row {
        row.validate(app, topic)?;
    }
    Ok(row)
}

async fn broadcast(
    tx: &Transaction,
    app: &AppId,
    id: &BroadcastId,
) -> Result<Broadcast, WorkflowServiceError> {
    let row = tx
        .database()
        .entity::<models::broadcasts::Entity>()?
        .find::<Broadcast>(
            models::broadcasts::app_id
                .eq(app.as_str())?
                .and(models::broadcasts::id.eq(id.as_str())?),
            one(),
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(invalid)?;
    row.validate(app, id)?;
    Ok(row)
}

fn one() -> FindOptions {
    FindOptions {
        limit: Some(1),
        ..Default::default()
    }
}
fn increment(value: i64, message: &str) -> Result<i64, WorkflowServiceError> {
    if value < 0 {
        return Err(invalid());
    }
    value
        .checked_add(1)
        .ok_or_else(|| WorkflowServiceError::ResourceExhausted(message.into()))
}
fn changed(count: i64) -> Result<(), WorkflowServiceError> {
    if count == 1 {
        Ok(())
    } else {
        Err(invalid())
    }
}
fn invalid() -> WorkflowServiceError {
    WorkflowServiceError::Internal("invalid workflow topic fanout journal".into())
}
