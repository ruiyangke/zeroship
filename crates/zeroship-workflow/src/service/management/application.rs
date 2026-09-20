use super::{
    target, AppStateLock, AppWorkflows, CapturedLease, Command, ManagementCommand, Transaction,
    WorkflowServiceError,
};
use crate::{
    operations::{RestartDeploy, RestartOptions},
    service::{
        control::{self, Preparation},
        delivery,
    },
};
use zeroship_core::{workflow_coordination::ManagementOutcome, workflow_jobs::DeploymentId};

pub(super) enum Prepared {
    Outcome(ManagementOutcome),
    Latest(DeploymentId),
}

pub(super) async fn prepare(
    scope: &AppWorkflows,
    tx: &mut Transaction,
    command: Command<'_>,
    authority: &CapturedLease,
    now: i64,
) -> Result<Prepared, WorkflowServiceError> {
    delivery::check_scope(scope.app_id(), command.job)?;
    tx.check_app(scope.app_id())?;
    authority.check(scope)?;
    match command.operation {
        ManagementCommand::Transition { operation } => {
            let outcome = match control::prepare_transition(
                tx,
                scope.app_id(),
                command.run_id.as_str(),
                *operation,
                authority.policy(),
                now,
            )
            .await?
            {
                Preparation::Ready(plan) => {
                    authority.check(scope)?;
                    ManagementOutcome::Applied {
                        state: plan
                            .apply(tx, scope.app_id(), command.run_id.as_str())
                            .await?
                            .state,
                    }
                }
                Preparation::Rejected(reason) => reason.outcome(),
            };
            Ok(Prepared::Outcome(outcome))
        }
        ManagementCommand::RestartStarted { from } => {
            let options = RestartOptions {
                from: from.clone(),
                deploy: Some(RestartDeploy::Started),
            };
            let outcome = match control::restart::prepare(
                tx,
                scope.app_id(),
                command.run_id.as_str(),
                &options,
                authority.policy(),
                now,
            )
            .await?
            {
                Preparation::Ready(plan) => {
                    authority.check(scope)?;
                    ManagementOutcome::Applied {
                        state: plan.apply().await?.state,
                    }
                }
                Preparation::Rejected(reason) => reason.outcome(),
            };
            Ok(Prepared::Outcome(outcome))
        }
        ManagementCommand::RestartLatest { deployment_id } => {
            match control::restart::prepare_draft(
                tx,
                scope.app_id(),
                command.run_id.as_str(),
                &RestartOptions::default(),
                authority.policy(),
                now,
            )
            .await?
            {
                Preparation::Ready(draft) => {
                    drop(draft);
                    Ok(Prepared::Latest(deployment_id.clone()))
                }
                Preparation::Rejected(reason) => Ok(Prepared::Outcome(reason.outcome())),
            }
        }
    }
}

pub(super) async fn latest(
    scope: &AppWorkflows,
    tx: &mut Transaction,
    lock: AppStateLock<'_>,
    command: Command<'_>,
    target: &target::Verified,
    authority: &CapturedLease,
    now: i64,
) -> Result<ManagementOutcome, WorkflowServiceError> {
    delivery::check_scope(scope.app_id(), command.job)?;
    tx.check_app(scope.app_id())?;
    authority.check(scope)?;
    target.install(tx, lock, command, now).await?;
    let draft = match control::restart::prepare_draft(
        tx,
        scope.app_id(),
        command.run_id.as_str(),
        &RestartOptions::default(),
        authority.policy(),
        now,
    )
    .await?
    {
        Preparation::Ready(draft) => draft,
        Preparation::Rejected(reason) => return Ok(reason.outcome()),
    };
    match draft.bind_exact(target.registration()).await? {
        Preparation::Ready(plan) => {
            authority.check(scope)?;
            Ok(ManagementOutcome::Applied {
                state: plan.apply().await?.state,
            })
        }
        Preparation::Rejected(reason) => Ok(reason.outcome()),
    }
}
