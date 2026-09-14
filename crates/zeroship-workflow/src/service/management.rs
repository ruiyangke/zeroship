//! Durable customer-side receipts for authenticated coordinator commands.

#![expect(
    clippy::future_not_send,
    reason = "Management transactions stay on their owning compio thread"
)]

use super::{
    app::{decode, encode, lock_app_state},
    control::{self, Preparation},
    models,
    policy::PolicyAuthority,
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
        let captured = self.capture_policy();
        captured
            .run(async {
                if command.app_id != self.app {
                    return Err(WorkflowServiceError::PermissionDenied);
                }
                let digest = digest(command)?;
                let mut tx = self.service.begin().await?;
                let authority = captured.authority();
                if authority.is_ok() {
                    lock_app_state(&mut tx, &self.app).await?;
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
                let outcome = Box::pin(
                    self.apply_management_authorized(&mut tx, command, &digest, authority?),
                )
                .await?;
                tx.commit().await?;
                Ok(outcome)
            })
            .await
    }

    /// Apply a fresh command inside the caller's app-locked transaction.
    /// The caller matches immutable receipts first and commits the resulting
    /// lifecycle, publication and receipt changes with its delivery bookkeeping.
    /// It must run the entire attempt through commit under the original captured
    /// authority, and abandon the transaction on any application or bookkeeping
    /// error. Checking authority only before this helper returns is insufficient.
    pub(in crate::service) async fn apply_management_authorized(
        &self,
        tx: &mut Transaction,
        command: &ManageRun,
        digest: &str,
        authority: &PolicyAuthority,
    ) -> Result<ManagementOutcome, WorkflowServiceError> {
        if command.app_id != self.app || !authority.belongs_to(&self.binding) {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        tx.check_app(&self.app)?;
        authority.check()?;
        let policy = &authority.policy;
        let now = tx.now().await?;
        let run_id = command.run_id.as_str();
        let outcome = match &command.command {
            ManagementOperation::Transition { operation } => {
                match control::prepare_transition(tx, &self.app, run_id, *operation, policy, now)
                    .await?
                {
                    Preparation::Ready(plan) => {
                        authority.check()?;
                        ManagementOutcome::Applied {
                            state: plan.apply(tx, &self.app, run_id).await?.state,
                        }
                    }
                    Preparation::Rejected(reason) => reason.outcome(),
                }
            }
            ManagementOperation::Restart { options } => {
                match control::restart::prepare(tx, &self.app, run_id, options, policy, now).await?
                {
                    Preparation::Ready(plan) => {
                        authority.check()?;
                        ManagementOutcome::Applied {
                            state: plan.apply(tx, &self.app, run_id, now).await?.state,
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
        authority.check()?;
        Ok(outcome)
    }
}
