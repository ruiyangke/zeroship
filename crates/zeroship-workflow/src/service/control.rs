#![expect(
    clippy::future_not_send,
    reason = "Lifecycle transactions stay on their owning compio thread"
)]

use super::{
    app::{lock_app, lock_run, parse_state, request_result, store_request, validate_run},
    models,
    store::Transaction,
    types::digest,
    AppPolicy, AppWorkflows, RequestId,
};
use crate::{
    operations::{RestartOptions, RestartedRun, RunOperation, RunState, TransitionedRun},
    WorkflowServiceError,
};
use zeroship_core::{app_id::AppId, workflow_coordination::ManagementOutcome};
use zeroship_data_orm::{orm::Entity, value, Value};

mod replay;
pub(super) mod restart;

/// Only lifecycle decisions can become durable rejections. ORM failures stay
/// outside this type, including permission failures from database operations.
pub(super) enum Rejection {
    NotFound,
    Conflict(String),
    Invalid(String),
    Denied,
}
impl Rejection {
    pub(super) const fn outcome(&self) -> ManagementOutcome {
        match self {
            Self::NotFound => ManagementOutcome::NotFound {},
            Self::Conflict(_) | Self::Invalid(_) => ManagementOutcome::Conflict {},
            Self::Denied => ManagementOutcome::Denied {},
        }
    }

    fn error(self) -> WorkflowServiceError {
        match self {
            Self::NotFound => WorkflowServiceError::NotFound("workflow run".into()),
            Self::Conflict(message) => WorkflowServiceError::Conflict(message),
            Self::Invalid(message) => WorkflowServiceError::InvalidRequest(message),
            Self::Denied => WorkflowServiceError::PermissionDenied,
        }
    }
}

pub(super) enum Preparation<T> {
    Ready(T),
    Rejected(Rejection),
}
impl<T> Preparation<T> {
    fn accept(self) -> Result<T, WorkflowServiceError> {
        match self {
            Self::Ready(plan) => Ok(plan),
            Self::Rejected(reason) => Err(reason.error()),
        }
    }
}

impl AppWorkflows {
    /// Change lifecycle intent, retaining the response for app request retries.
    ///
    /// # Errors
    /// Rejects unknown runs, invalid transitions and reused request identities;
    /// reports admission and journal failures.
    pub async fn transition(
        &self,
        request: &RequestId,
        run_id: &str,
        operation: RunOperation,
    ) -> Result<TransitionedRun, WorkflowServiceError> {
        validate_run(run_id)?;
        let digest = digest(&(run_id, operation))?;
        let mut tx = self.service.begin().await?;
        let policy = lock_app(&mut tx, &self.app).await?;
        let now = tx.now().await?;
        if let Some(receipt) =
            request_result(&tx, &self.app, request, "transition", &digest).await?
        {
            return Ok(receipt);
        }
        let plan = prepare_transition(&mut tx, &self.app, run_id, operation, &policy, now)
            .await?
            .accept()?;
        let result = plan.apply(&tx, &self.app, run_id).await?;
        store_request(
            &mut tx,
            &self.app,
            request,
            "transition",
            &digest,
            &result,
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Restart from a replay boundary while preserving retained step effects.
    ///
    /// # Errors
    /// Rejects unsafe restart boundaries, live execution and reused request
    /// identities; reports admission, deployment and journal failures.
    pub async fn restart(
        &self,
        request: &RequestId,
        run_id: &str,
        options: RestartOptions,
    ) -> Result<RestartedRun, WorkflowServiceError> {
        validate_run(run_id)?;
        crate::lifecycle::restart_deploy_policy(&options)?;
        let digest = digest(&(run_id, &options))?;
        let mut tx = self.service.begin().await?;
        let policy = lock_app(&mut tx, &self.app).await?;
        let now = tx.now().await?;
        if let Some(receipt) = request_result(&tx, &self.app, request, "restart", &digest).await? {
            return Ok(receipt);
        }
        let plan = restart::prepare(&mut tx, &self.app, run_id, &options, &policy, now)
            .await?
            .accept()?;
        let result = plan.apply(&mut tx, &self.app, run_id, now).await?;
        store_request(
            &mut tx, &self.app, request, "restart", &digest, &result, now,
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }
}

pub(super) struct TransitionPlan {
    state: RunState,
    patch: Option<Value>,
    advance: bool,
    now: i64,
}

pub(super) async fn prepare_transition(
    tx: &mut Transaction,
    app: &AppId,
    run_id: &str,
    operation: RunOperation,
    policy: &AppPolicy,
    now: i64,
) -> Result<Preparation<TransitionPlan>, WorkflowServiceError> {
    let run = match lock_run(tx, app, run_id).await {
        Ok(run) => run,
        Err(WorkflowServiceError::NotFound(_)) => {
            return Ok(Preparation::Rejected(Rejection::NotFound));
        }
        Err(error) => return Err(error),
    };
    let mut state = parse_state(&run.text("state")?)?;
    let patch = if state.is_terminal() {
        if operation != RunOperation::Cancel {
            return Ok(Preparation::Rejected(Rejection::Conflict(
                "workflow run is terminal".into(),
            )));
        }
        None
    } else {
        let leased = run.optional_text("task_id")?.is_some();
        Some(match operation {
            RunOperation::Pause if leased => value!({"control":"pause"}),
            RunOperation::Pause => {
                state = RunState::Paused;
                value!({"control":"pause", "state":"paused", "due_at":null, "task_id":null})
            }
            RunOperation::Resume => {
                if policy.admit().is_err() {
                    return Ok(Preparation::Rejected(Rejection::Denied));
                }
                if leased {
                    value!({"control":"none"})
                } else {
                    let phase = if run.optional_text("compensation_target")?.is_some() {
                        "compensating"
                    } else {
                        "queued"
                    };
                    state = parse_state(phase)?;
                    value!({"control":"none", "state":phase, "due_at":now})
                }
            }
            RunOperation::Cancel if leased => value!({"control":"cancel"}),
            RunOperation::Cancel => value!({"control":"cancel", "due_at":now}),
        })
    };
    let advance = patch.is_some() && run.optional_text("task_id")?.is_none();
    Ok(Preparation::Ready(TransitionPlan {
        state,
        patch,
        advance,
        now,
    }))
}

impl TransitionPlan {
    pub(super) async fn apply(
        self,
        tx: &Transaction,
        app: &AppId,
        run_id: &str,
    ) -> Result<TransitionedRun, WorkflowServiceError> {
        if let Some(patch) = self.patch {
            tx.database()
                .collection(models::runs::Entity::COLLECTION)?
                .update(value!({"app_id":app.as_str(), "id":run_id}), patch)
                .await?;
            if self.advance {
                super::publication::advance(tx, app, run_id, self.now).await?;
            }
        }
        Ok(TransitionedRun { state: self.state })
    }
}
