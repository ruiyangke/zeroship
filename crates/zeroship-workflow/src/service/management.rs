//! Durable customer-side receipts for authenticated coordinator commands.

#![expect(
    clippy::future_not_send,
    reason = "Management transactions stay on their owning compio thread"
)]

use super::{
    app::{decode, encode, lock_app},
    control::{self, Preparation},
    models,
    policy::{CapturedPolicy, PolicyAuthority},
    store::Transaction,
    types::{digest, storage_id},
    AppWorkflows,
};
use crate::WorkflowServiceError;
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
        let captured = CapturedPolicy::capture(&self.service.policies, &self.app);
        captured
            .run(async {
                if command.app_id != self.app {
                    return Err(WorkflowServiceError::PermissionDenied);
                }
                let digest = digest(command)?;
                let mut tx = self.service.begin().await?;
                let authority = captured.authority();
                if authority.is_ok() {
                    lock_app(&mut tx, &self.app).await?;
                }
                // Immutable receipts can be replayed without fresh mutation authority.
                // A missing receipt must still fail with the original captured error.
                let stored = tx
                    .database()
                    .entity::<models::management_receipts::Entity>()?
                    .find::<models::ManagementResult>(
                        models::management_receipts::app_id
                            .eq(self.app.as_str())?
                            .and(
                                models::management_receipts::request_id
                                    .eq(command.request_id.as_str())?,
                            ),
                        FindOptions {
                            limit: Some(1),
                            ..Default::default()
                        },
                    )
                    .await?;
                if let Some(receipt) = stored.into_iter().next() {
                    if receipt.digest != digest {
                        return Err(WorkflowServiceError::Conflict(
                            "workflow management request was reused with a different command"
                                .into(),
                        ));
                    }
                    return decode(&receipt.outcome);
                }
                Box::pin(self.apply_management_authorized(tx, command, &digest, authority?)).await
            })
            .await
    }

    async fn apply_management_authorized(
        &self,
        mut tx: Transaction,
        command: &ManageRun,
        digest: &str,
        authority: &PolicyAuthority,
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
