use super::{app::encode, AppPolicy, DeployRegistration};
use crate::{validation, WorkflowServiceError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub use zeroship_workflow_calendar::{
    IntervalAnchor, ScheduleCatchUp, ScheduleOverlap, ScheduleTiming,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScheduleRegistration {
    pub name: String,
    pub workflow_name: String,
    pub schedule: ScheduleTiming,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub overlap: ScheduleOverlap,
    #[serde(default)]
    pub catch_up: ScheduleCatchUp,
}

impl ScheduleRegistration {
    fn validate(
        &self,
        deploy: &DeployRegistration,
        policy: &AppPolicy,
        now: i64,
    ) -> Result<(), WorkflowServiceError> {
        validation::workflow_name(&self.name)?;
        validation::workflow_name(&self.workflow_name)?;
        if !deploy.workflows.contains(&self.workflow_name) {
            return Err(WorkflowServiceError::InvalidRequest(
                "scheduled workflow is absent from the deployment".into(),
            ));
        }
        if encode(&self.input)?.len() > policy.max_input_bytes {
            return Err(WorkflowServiceError::PayloadTooLarge);
        }
        if let ScheduleCatchUp::Backfill { max } = self.catch_up {
            if max == 0 || max > policy.max_schedule_backfill {
                return Err(WorkflowServiceError::InvalidRequest(
                    "schedule backfill exceeds the app limit".into(),
                ));
            }
        }
        if let ScheduleTiming::Interval { interval_ms, .. } = &self.schedule {
            if *interval_ms < policy.min_schedule_interval_ms {
                return Err(WorkflowServiceError::InvalidRequest(
                    "schedule interval is below the app limit".into(),
                ));
            }
        }
        self.schedule.next_after(now, now)?;
        Ok(())
    }
}

pub(super) fn validate_deployment(
    deploy: &DeployRegistration,
    policy: &AppPolicy,
    now: i64,
) -> Result<(), WorkflowServiceError> {
    if deploy.schedules.len() > policy.max_schedules {
        return Err(WorkflowServiceError::ResourceExhausted(
            "workflow schedule limit reached".into(),
        ));
    }
    let mut names = std::collections::BTreeSet::new();
    for registration in &deploy.schedules {
        registration.validate(deploy, policy, now)?;
        if !names.insert(&registration.name) {
            return Err(WorkflowServiceError::InvalidRequest(
                "workflow schedule names must be unique".into(),
            ));
        }
    }
    Ok(())
}
