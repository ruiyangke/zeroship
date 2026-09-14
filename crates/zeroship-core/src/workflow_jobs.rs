//! Closed metadata for durable workflow delivery across database zones.
//!
//! Customer inputs, history and outputs stay in creator storage. The manager
//! validates app scope, execution authority and successor bounds separately.

pub use zeroship_id::workflow::{BroadcastId, DeploymentId, JobId};

use crate::{
    app_id::AppId,
    workflow_coordination::{
        AssignedScope, ManagementOutcome, RequestId, RestartTarget, Revision, RunId, RunOperation,
        UnixMillis, WorkerId,
    },
    workflow_schedules::ScheduleId,
};
use serde::{Deserialize, Serialize};
use std::num::NonZeroU64;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum JobOperation {
    Activate {
        deployment_id: DeploymentId,
        revision: Revision,
    },
    Advance {
        deployment_id: DeploymentId,
        run_id: RunId,
        generation: u32,
        revision: Revision,
    },
    Cron {
        deployment_id: DeploymentId,
        schedule_id: ScheduleId,
        schedule_name: String,
        request_id: RequestId,
        run_id: RunId,
        revision: Revision,
        scheduled_at: UnixMillis,
    },
    Management {
        request_id: RequestId,
        run_id: RunId,
        revision: Revision,
        command: ManagementCommand,
    },
    Fanout {
        broadcast_id: BroadcastId,
        revision: Revision,
    },
    // Empty struct variants reject extra fields on internally tagged messages.
    Reconcile {},
    Collect {},
}

/// The effective lifecycle command frozen by trusted manager acceptance.
///
/// A latest restart cannot carry a retained task boundary. A started restart
/// resolves its current source generation inside the creator transaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ManagementCommand {
    Transition {
        operation: RunOperation,
    },
    RestartStarted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<RestartTarget>,
    },
    RestartLatest {
        deployment_id: DeploymentId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobSpec {
    pub id: JobId,
    pub app_id: AppId,
    pub operation: JobOperation,
    pub available_at: UnixMillis,
}

impl JobSpec {
    /// The operation's executable prerequisite, if it has one. Journal-only
    /// operations must remain deliverable without acquiring a deployment hold.
    #[must_use]
    pub const fn deployment_id(&self) -> Option<&DeploymentId> {
        match &self.operation {
            JobOperation::Activate { deployment_id, .. }
            | JobOperation::Advance { deployment_id, .. }
            | JobOperation::Cron { deployment_id, .. }
            | JobOperation::Management {
                command: ManagementCommand::RestartLatest { deployment_id },
                ..
            } => Some(deployment_id),
            JobOperation::Management {
                command:
                    ManagementCommand::Transition { .. } | ManagementCommand::RestartStarted { .. },
                ..
            }
            | JobOperation::Fanout { .. }
            | JobOperation::Reconcile {}
            | JobOperation::Collect {} => None,
        }
    }
}

/// Worker publication carries placement identity, never a caller-chosen expiry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubmitJob {
    pub scope: AssignedScope,
    pub job: JobSpec,
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

/// Remaining manager authority transferred without comparing database-zone clocks.
///
/// The receiver anchors this duration before starting its request and rejects
/// replies whose resulting monotonic deadline has already expired.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliveryLease {
    pub delivery: Delivery,
    pub remaining_ms: NonZeroU64,
}

/// Host-authorized delivery with a deadline on the host's monotonic clock.
///
/// Native manager grants and authenticated client replies implement this seam;
/// a serialized delivery alone cannot supply execution authority. Implementors
/// must preserve the original expiration when cloned or repeatedly observed.
pub trait JobLease {
    fn delivery(&self) -> &Delivery;
    fn remaining(&self) -> Option<Duration>;
}

/// Scheduling classification without customer results or free-form failures.
///
/// For Fanout, `Waiting` confirms a committed successor page and `Completed`
/// finishes the broadcast expansion. For reconciliation and collection, `Waiting`
/// requests another scan page or phase and `Completed` closes that scan cycle.
/// Neither classification asserts that the manager queue or app intents drained.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum JobOutcome {
    Completed {},
    Waiting {},
    Rejected {},
    Management { outcome: ManagementOutcome },
}

impl JobOutcome {
    /// Match the outcome family to its operation. Creator handlers separately
    /// enforce their lifecycle rules; this check grants no execution authority.
    #[must_use]
    pub const fn valid_for(&self, operation: &JobOperation) -> bool {
        match (self, operation) {
            (Self::Management { .. }, JobOperation::Management { .. }) => true,
            (Self::Management { .. }, _) | (_, JobOperation::Management { .. }) => false,
            (Self::Completed {} | Self::Waiting {} | Self::Rejected {}, _) => true,
        }
    }
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
