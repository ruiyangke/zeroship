//! Ordered manager-delivered lifecycle application in the customer journal.

#![expect(
    clippy::future_not_send,
    reason = "management holds compio-local journal transactions and deployment clients"
)]

use super::{
    app::{lock_app_state, AppStateLock},
    delivery::{self, CapturedLease, JobReceipt},
    store::Transaction,
    AppWorkflows,
};
use crate::WorkflowServiceError;
use zeroship_core::{
    workflow_coordination::{RequestId, RunId},
    workflow_jobs::{JobLease, JobOperation, JobSpec, ManagementCommand},
};

mod application;
mod history;
mod target;

#[derive(Clone, Copy)]
struct Command<'a> {
    job: &'a JobSpec,
    request_id: &'a RequestId,
    run_id: &'a RunId,
    revision: i64,
    operation: &'a ManagementCommand,
}

impl<'a> Command<'a> {
    fn read(job: &'a JobSpec) -> Result<Self, WorkflowServiceError> {
        let JobOperation::Management {
            request_id,
            run_id,
            revision,
            command,
        } = &job.operation
        else {
            return Err(WorkflowServiceError::InvalidRequest(
                "expected workflow management job".into(),
            ));
        };
        Ok(Self {
            job,
            request_id,
            run_id,
            revision: revision.get(),
            operation: command,
        })
    }
}

impl AppWorkflows {
    /// Apply the exact ordered lifecycle command carried by a manager delivery.
    /// Committed results replay without fresh authority or deployment I/O.
    ///
    /// # Errors
    /// Refuses foreign or changed jobs, order gaps, expired authority and damaged
    /// journal or deployment state. Transient failures retain no lifecycle refusal.
    pub async fn management_job(
        &self,
        lease: &impl JobLease,
    ) -> Result<JobReceipt, WorkflowServiceError> {
        let job = &lease.delivery().job;
        delivery::check_scope(self.app_id(), job)?;
        let command = Command::read(job)?;
        let captured = CapturedLease::capture(self, lease)
            .and_then(|authority| Ok((authority.bind(self)?, authority)));
        let budget =
            delivery::attempt_budget(captured.as_ref().ok().map(|(_, authority)| authority), None);
        let policy = captured
            .as_ref()
            .ok()
            .map(|(scope, _)| scope.capture_policy());
        let operation = delivery::run_attempt(
            None,
            budget,
            Box::pin(async {
                let mut tx = match &captured {
                    Ok((scope, _)) => scope.service.begin().await?,
                    Err(_) => self.service.begin_history().await?,
                };
                lock_app_state(&mut tx, self.app_id()).await?;
                let state = history::inspect(&tx, command).await?;
                if let history::Observed::Replay(receipt) = state {
                    tx.commit().await?;
                    return Ok(*receipt);
                }
                let (scope, authority) = captured?;
                authority.check(&scope)?;
                state.require_next(command.revision)?;
                tx.capture_mutation(scope.app_id())?;
                let now = tx.now().await?;
                let prepared =
                    application::prepare(&scope, &mut tx, command, &authority, now).await?;
                match prepared {
                    application::Prepared::Outcome(outcome) => {
                        let receipt = history::finish(&tx, command, outcome, now).await?;
                        authority.check(&scope)?;
                        tx.commit().await?;
                        Ok(receipt)
                    }
                    application::Prepared::Latest(deployment) => {
                        authority.check(&scope)?;
                        tx.commit().await?;
                        let target = target::load(&scope, &deployment, &authority).await?;
                        Box::pin(scope.apply_latest(command, &authority, &target)).await
                    }
                }
            }),
        );
        match policy {
            Some(policy) => policy.run(operation).await,
            None => operation.await,
        }
    }

    async fn apply_latest(
        &self,
        command: Command<'_>,
        authority: &CapturedLease,
        target: &target::Verified,
    ) -> Result<JobReceipt, WorkflowServiceError> {
        let mut tx = self.service.begin().await?;
        let lock = lock_app_state(&mut tx, self.app_id()).await?;
        let state = history::inspect(&tx, command).await?;
        if let history::Observed::Replay(receipt) = state {
            tx.commit().await?;
            return Ok(*receipt);
        }
        authority.check(self)?;
        state.require_next(command.revision)?;
        tx.capture_mutation(self.app_id())?;
        let now = tx.now().await?;
        let outcome =
            application::latest(self, &mut tx, lock, command, target, authority, now).await?;
        let receipt = history::finish(&tx, command, outcome, now).await?;
        authority.check(self)?;
        tx.commit().await?;
        Ok(receipt)
    }
}

pub(super) async fn receipt(
    tx: &Transaction,
    job: &JobSpec,
) -> Result<Option<JobReceipt>, WorkflowServiceError> {
    match history::inspect(tx, Command::read(job)?).await? {
        history::Observed::Replay(receipt) => Ok(Some(*receipt)),
        history::Observed::Fresh(_) => Ok(None),
    }
}

fn invalid() -> WorkflowServiceError {
    WorkflowServiceError::Internal("invalid workflow management journal".into())
}

fn conflict() -> WorkflowServiceError {
    WorkflowServiceError::Conflict("workflow management job identity or order conflicts".into())
}
