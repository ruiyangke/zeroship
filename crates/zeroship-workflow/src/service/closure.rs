//! Manager-origin closure of one ingress epoch in the assigned app.
//!
//! A delivered Close raises the journal's closed epoch and evaluates the closed
//! drain predicates in one transaction under the app state lock. Ingress
//! acceptance takes the same lock and requires its captured epoch to exceed the
//! closed epoch, so acceptance and closure serialize in both orders: acceptance
//! first makes the evidence undrained, and Close first fences the acceptance.
//! The fence commits even when the evidence is undrained.

#![expect(
    clippy::future_not_send,
    reason = "closure owns compio-local journal operations"
)]

use super::{
    app::{closed_epoch, encode, lock_app_state},
    delivery::{self, CapturedLease, JobReceipt},
    models::{app_state, deployment_holds, job_publications, job_receipts, payloads, tasks},
    store::Transaction,
    AppWorkflows,
};
use crate::WorkflowServiceError;
use zeroship_core::{
    app_id::AppId,
    workflow_jobs::{JobLease, JobOperation, JobOutcome, JobSpec},
};
use zeroship_data_orm::orm::{FromRow, Insertable};

#[derive(Insertable)]
#[orm(entity = job_receipts)]
struct PendingReceipt {
    id: String,
    app_id: String,
    specification: String,
    created_at: i64,
}

#[derive(FromRow)]
#[orm(entity = job_receipts)]
struct ReceiptId {
    id: String,
}

impl AppWorkflows {
    /// Fence the job's ingress epoch and report whether the app drained.
    ///
    /// Closure is journal-only and runs under any live policy, including one
    /// that disables admission, dispatch and ingress. An exact committed receipt
    /// replays without fresh authority, so a lost acknowledgement or a crash
    /// after commit converges on the original evidence.
    ///
    /// # Errors
    /// Refuses foreign or non-closure jobs, exhausted authority and journal
    /// failures. A failed attempt commits neither the fence nor the evidence.
    pub async fn close_job(
        &self,
        grant: &impl JobLease,
    ) -> Result<JobReceipt, WorkflowServiceError> {
        let job = &grant.delivery().job;
        delivery::check_scope(self.app_id(), job)?;
        let JobOperation::Close { epoch } = job.operation else {
            return Err(WorkflowServiceError::InvalidRequest(
                "expected workflow closure job".into(),
            ));
        };
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
                if let Some(receipt) = receipt(&tx, job).await? {
                    tx.commit().await?;
                    return Ok(receipt);
                }
                let (scope, authority) = captured.as_ref().map_err(Clone::clone)?;
                authority.check(scope)?;
                tx.capture_mutation(self.app_id())?;
                let now = tx.now().await?;
                raise_closed_epoch(&tx, self.app_id(), epoch.get()).await?;
                let drained = drained(&tx, self.app_id(), now).await?;
                let pending = tx
                    .database()
                    .entity::<job_receipts::Entity>()?
                    .insert::<_, ReceiptId>(PendingReceipt {
                        id: job.id.as_str().to_owned(),
                        app_id: self.app_id().as_str().to_owned(),
                        specification: encode(job)?,
                        created_at: now,
                    })
                    .await?;
                if pending.id != job.id.as_str() {
                    return Err(invalid());
                }
                let receipt =
                    delivery::finish(&tx, job, JobOutcome::Closed { drained }, now).await?;
                authority.check(scope)?;
                tx.commit().await?;
                Ok(receipt)
            }),
        );
        match policy {
            Some(policy) => policy.run(operation).await,
            None => operation.await,
        }
    }
}

/// Replay an exact committed closure. A closure record is created and settled in
/// the same transaction, so an unsettled record is damage, not work in progress.
pub(super) async fn receipt(
    tx: &Transaction,
    job: &JobSpec,
) -> Result<Option<JobReceipt>, WorkflowServiceError> {
    if !matches!(job.operation, JobOperation::Close { .. }) {
        return Err(invalid());
    }
    let Some(record) = delivery::read(tx, job).await? else {
        return Ok(None);
    };
    record.receipt(job)?.map(Some).ok_or_else(invalid)
}

/// The closed epoch never moves backwards. A stale Close keeps the newer fence
/// and still reports evidence for the current journal state.
async fn raise_closed_epoch(
    tx: &Transaction,
    app: &AppId,
    epoch: i64,
) -> Result<(), WorkflowServiceError> {
    let closed = closed_epoch(tx, app).await?;
    if epoch <= closed {
        return Ok(());
    }
    let changed = tx
        .database()
        .entity::<app_state::Entity>()?
        .update_many(
            app_state::app_id
                .eq(app.as_str())?
                .and(app_state::closed_epoch.eq(closed)?),
            app_state::closed_epoch.set(epoch)?,
        )
        .await?;
    if changed != 1 {
        return Err(invalid());
    }
    Ok(())
}

/// Closed drain evidence, read under the app state lock in the fencing
/// transaction: no unconfirmed publication intent, no deployment hold in
/// transition, no payload in preparation or deletion, no deletion tombstone
/// inside its resweep window and no live task claim.
pub(super) async fn drained(
    tx: &Transaction,
    app: &AppId,
    now: i64,
) -> Result<bool, WorkflowServiceError> {
    let db = tx.database();
    let unconfirmed = db
        .entity::<job_publications::Entity>()?
        .exists(
            job_publications::app_id
                .eq(app.as_str())?
                .and(job_publications::confirmed_at.is_null()),
        )
        .await?;
    let holds = db
        .entity::<deployment_holds::Entity>()?
        .exists(
            deployment_holds::app_id
                .eq(app.as_str())?
                .and(deployment_holds::state.in_values(["acquiring", "releasing"])?),
        )
        .await?;
    let payload_work = db
        .entity::<payloads::Entity>()?
        .exists(
            payloads::app_id
                .eq(app.as_str())?
                .and(payloads::state.in_values(["uploading", "staged", "deleting"])?),
        )
        .await?;
    let tombstones = db
        .entity::<payloads::Entity>()?
        .exists(
            payloads::app_id
                .eq(app.as_str())?
                .and(payloads::state.eq("deleted")?)
                .and(payloads::expires_at.gt(now)?),
        )
        .await?;
    let claims = db
        .entity::<tasks::Entity>()?
        .exists(
            tasks::app_id
                .eq(app.as_str())?
                .and(tasks::state.eq("leased")?)
                .and(tasks::deadline.gt(now)?),
        )
        .await?;
    Ok(!(unconfirmed || holds || payload_work || tombstones || claims))
}

fn invalid() -> WorkflowServiceError {
    WorkflowServiceError::Internal("invalid workflow closure journal".into())
}
