//! Closed metadata for durable workflow delivery across database zones.
//!
//! Customer inputs, history and outputs stay in creator storage. The manager
//! validates app scope, execution authority and successor bounds separately.

pub use zeroship_id::workflow::{DeploymentId, JobId};

use crate::{
    app_id::AppId,
    workflow_coordination::{RequestId, Revision, RunId, UnixMillis, WorkerId},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum JobOperation {
    Advance {
        run_id: RunId,
        generation: u32,
        revision: Revision,
    },
    Cron {
        request_id: RequestId,
        run_id: RunId,
        revision: Revision,
        scheduled_at: UnixMillis,
    },
    Management {
        request_id: RequestId,
        run_id: RunId,
    },
    // Empty struct variants reject extra fields on internally tagged messages.
    Reconcile {},
    Collect {},
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobSpec {
    pub id: JobId,
    pub app_id: AppId,
    pub deployment_id: DeploymentId,
    pub operation: JobOperation,
    pub available_at: UnixMillis,
}

/// A delivery lease does not replace the creator journal's execution fence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Delivery {
    pub job: JobSpec,
    pub worker_id: WorkerId,
    pub assignment_revision: Revision,
    pub attempt: Revision,
    pub deadline: UnixMillis,
}

/// Scheduling classification without customer results or free-form failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobOutcome {
    Completed,
    Waiting,
    Rejected,
}

/// Successors use the same stable identities when published through an outbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settlement {
    pub delivery: Delivery,
    pub outcome: JobOutcome,
    pub successors: Vec<JobSpec>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettlementReceipt {
    pub job_id: JobId,
    pub app_id: AppId,
    pub attempt: Revision,
    pub outcome: JobOutcome,
}
