//! Closed deployment scheduling metadata; business input stays in the app bundle.

use crate::{app_id::AppId, workflow_coordination::Revision, workflow_jobs::DeploymentId};
use serde::{Deserialize, Serialize};
pub use zeroship_id::workflow::ScheduleId;
pub use zeroship_workflow_calendar::{ScheduleCatchUp, ScheduleOverlap, ScheduleTiming};

/// The deployment host projects the normal manifest onto this allowlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScheduleDescriptor {
    pub name: String,
    pub workflow_name: String,
    pub schedule: ScheduleTiming,
    #[serde(default)]
    pub overlap: ScheduleOverlap,
    #[serde(default)]
    pub catch_up: ScheduleCatchUp,
}

/// Preparation records immutable metadata and does not activate scheduling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegisterSchedules {
    pub app_id: AppId,
    pub deployment_id: DeploymentId,
    pub schedules: Vec<ScheduleDescriptor>,
}

/// The platform chooses a monotonic activation revision; retries preserve it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActivateSchedules {
    pub app_id: AppId,
    pub deployment_id: DeploymentId,
    pub revision: Revision,
}

/// Stop future calendar generation without cancelling accepted work.
/// The platform shares the activation revision sequence; acceptance echoes the
/// exact command, including when a later activation has restored scheduling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DisableSchedules {
    pub app_id: AppId,
    pub revision: Revision,
}
