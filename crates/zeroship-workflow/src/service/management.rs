//! Durable customer-side receipts for authenticated coordinator commands.

use super::{
    app::{decode, encode, lock_app},
    control::{self, Preparation},
    models,
    policy::ManagementAuthority,
    store::Transaction,
    types::{digest, storage_id},
    AppWorkflows,
};
use crate::WorkflowServiceError;
use std::time::Instant;
use zeroship_core::workflow_coordination::{ManageRun, ManagementOperation, ManagementOutcome};
use zeroship_data_orm::{
    orm::{Entity, FindOptions},
    value,
};

impl AppWorkflows {
    /// Apply an authenticated coordinator command to this app's journal.
    ///
    /// The host authorizes the command before binding this handle. Receipts are
    /// retained independently of app request retention, including rejected
    /// lifecycle decisions. Neither this operation nor its receipt needs access
    /// to the coordinator database.
    ///
    /// # Errors
    /// Refuses a foreign app or changed request body. Missing host authority,
    /// storage failures and exhausted capacity remain retryable without a receipt.
    pub async fn apply_management(
        &self,
        command: &ManageRun,
    ) -> Result<ManagementOutcome, WorkflowServiceError> {
        if command.app_id != self.app {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        let digest = digest(command)?;
        let mut tx = self.service.begin().await?;
        lock_app(&mut tx, &self.app).await?;
        let stored = tx
            .database()
            .entity::<models::management_receipts::Entity>()?
            .find::<models::ManagementResult>(
                models::management_receipts::app_id
                    .eq(self.app.as_str())?
                    .and(models::management_receipts::request_id.eq(command.request_id.as_str())?),
                FindOptions {
                    limit: Some(1),
                    ..Default::default()
                },
            )
            .await?;
        if let Some(receipt) = stored.into_iter().next() {
            if receipt.digest != digest {
                return Err(WorkflowServiceError::Conflict(
                    "workflow management request was reused with a different command".into(),
                ));
            }
            return decode(&receipt.outcome);
        }
        let authority = self.service.policies.management_authority(&self.app)?;
        let deadline = authority.deadline;
        let attempt = self.apply_management_authorized(tx, command, &digest, &authority);
        if let Some(deadline) = deadline {
            // Cancellation covers lifecycle mutation and settlement. If a
            // commit's acknowledgement is lost, the next host reads its receipt.
            compio::time::timeout(deadline.saturating_duration_since(Instant::now()), attempt)
                .await
                .map_err(|_| {
                    WorkflowServiceError::Unavailable(
                        "workflow management authority expired".into(),
                    )
                })?
        } else {
            attempt.await
        }
    }

    async fn apply_management_authorized(
        &self,
        mut tx: Transaction,
        command: &ManageRun,
        digest: &str,
        authority: &ManagementAuthority,
    ) -> Result<ManagementOutcome, WorkflowServiceError> {
        authority.check(&self.service.policies, &self.app)?;
        let policy = &authority.policy;
        let now = tx.now().await?;
        let run_id = command.run_id.as_str();
        let outcome = match &command.command {
            ManagementOperation::Transition { operation } => {
                match control::prepare_transition(
                    &mut tx, &self.app, run_id, *operation, policy, now,
                )
                .await?
                {
                    Preparation::Ready(plan) => {
                        authority.check(&self.service.policies, &self.app)?;
                        ManagementOutcome::Applied {
                            state: plan.apply(&tx, &self.app, run_id).await?.state,
                        }
                    }
                    Preparation::Rejected(reason) => reason.outcome(),
                }
            }
            ManagementOperation::Restart { options } => {
                match control::restart::prepare(&mut tx, &self.app, run_id, options, policy, now)
                    .await?
                {
                    Preparation::Ready(plan) => {
                        authority.check(&self.service.policies, &self.app)?;
                        ManagementOutcome::Applied {
                            state: plan.apply(&mut tx, &self.app, run_id, now).await?.state,
                        }
                    }
                    Preparation::Rejected(reason) => reason.outcome(),
                }
            }
        };
        tx.database()
            .collection(models::management_receipts::Entity::COLLECTION)?
            .insert(value!({
                "id":storage_id(), "app_id":self.app.as_str(),
                "request_id":command.request_id.as_str(), "digest":digest,
                "outcome":encode(&outcome)?, "created_at":now,
            }))
            .await?;
        authority.check(&self.service.policies, &self.app)?;
        tx.commit().await?;
        Ok(outcome)
    }
}
