//! Closed metadata for durable workflow delivery across database zones.
//!
//! Customer inputs, history and outputs stay in creator storage. The manager
//! validates app scope, execution authority and successor bounds separately.

pub use zeroship_id::workflow::{DeploymentId, JobId};

use crate::{
    app_id::AppId,
    workflow_coordination::{AssignedScope, RequestId, Revision, RunId, UnixMillis, WorkerId},
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
        revision: Revision,
    },
    Advance {
        run_id: RunId,
        generation: u32,
        revision: Revision,
    },
    Cron {
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
/// For reconciliation, `Waiting` requests another page or intent phase and
/// `Completed` closes the scan cycle. Neither result proves that intents drained.
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
