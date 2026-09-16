use super::*;
use crate::service::AppWorkflows;
use zeroship_core::{
    workflow_coordination::WorkerId,
    workflow_jobs::{
        Delivery, DeploymentId, JobId, JobLease, JobOperation, JobOutcome, JobSpec,
        ManagementCommand,
    },
};

#[derive(Clone)]
pub struct Grant {
    pub delivery: Delivery,
    pub expires: Instant,
}

impl Grant {
    pub fn new(app: &AppId, run: &str, revision: i64, command: ManagementCommand) -> Self {
        Self {
            delivery: Delivery {
                job: JobSpec {
                    id: JobId::mint(),
                    app_id: app.clone(),
                    operation: JobOperation::Management {
                        request_id: RequestId::mint(),
                        run_id: RunId::parse(run).unwrap(),
                        revision: revision.try_into().unwrap(),
                        command,
                    },
                    available_at: 0.try_into().unwrap(),
                },
                worker_id: WorkerId::mint(),
                assignment_revision: 1.try_into().unwrap(),
                attempt: 1.try_into().unwrap(),
                deadline: 0.try_into().unwrap(),
            },
            expires: Instant::now() + Duration::from_secs(30),
        }
    }

    pub fn retry(&self) -> Self {
        let mut retry = self.clone();
        retry.delivery.attempt = (self.delivery.attempt.get() + 1).try_into().unwrap();
        retry.expires = Instant::now() + Duration::from_secs(30);
        retry
    }

    pub fn request_id(&self) -> &RequestId {
        let JobOperation::Management { request_id, .. } = &self.delivery.job.operation else {
            panic!("management fixture operation");
        };
        request_id
    }

    pub fn run_id(&self) -> &RunId {
        let JobOperation::Management { run_id, .. } = &self.delivery.job.operation else {
            panic!("management fixture operation");
        };
        run_id
    }

    pub fn command_mut(&mut self) -> &mut ManagementCommand {
        let JobOperation::Management { command, .. } = &mut self.delivery.job.operation else {
            panic!("management fixture operation");
        };
        command
    }
}

impl JobLease for Grant {
    fn delivery(&self) -> &Delivery {
        &self.delivery
    }

    fn remaining(&self) -> Option<Duration> {
        self.expires
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
    }
}

pub fn started(app: &AppId, run: &str, revision: i64) -> Grant {
    Grant::new(
        app,
        run,
        revision,
        ManagementCommand::RestartStarted { from: None },
    )
}

pub fn latest(app: &AppId, run: &str, revision: i64, deployment: &DeployRegistration) -> Grant {
    Grant::new(
        app,
        run,
        revision,
        ManagementCommand::RestartLatest {
            deployment_id: DeploymentId::parse(&deployment.id).unwrap(),
        },
    )
}

pub fn transition(app: &AppId, run: &str, revision: i64, operation: RunOperation) -> Grant {
    Grant::new(
        app,
        run,
        revision,
        ManagementCommand::Transition { operation },
    )
}

impl AppWorkflows {
    #[expect(
        clippy::future_not_send,
        reason = "management fixtures use the owning compio journal"
    )]
    pub(crate) async fn management_outcome(
        &self,
        grant: &Grant,
    ) -> Result<ManagementOutcome, WorkflowServiceError> {
        let receipt = self.management_job(grant).await?;
        assert_eq!(receipt.job, grant.delivery.job);
        let JobOutcome::Management { outcome } = receipt.outcome else {
            panic!("management must return its closed lifecycle outcome");
        };
        Ok(outcome)
    }
}
