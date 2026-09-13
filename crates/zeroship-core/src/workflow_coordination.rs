//! Metadata exchanged with the workflow coordinator.
//!
//! Execution inputs, journal records, customer connection information and
//! payload descriptors belong to customer-worker contracts, not this protocol.
//! Service authentication establishes the caller; IDs in messages select
//! resources and never grant authority to them.

mod lifecycle;
pub use lifecycle::{RestartDeploy, RestartOptions, RestartTarget, RunOperation, RunState};

use crate::{app_id::AppId, entity_id::declare_entity_id, typed_id};
use serde::{Deserialize, Serialize};
use std::num::{NonZeroI64, NonZeroU32};

pub const AUDIENCE: &str = "spiffe://zeroship.ai/svc/workflow";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    Invalid,
    Unauthenticated,
    RequestTooLarge,
    Denied,
    Conflict,
    Capacity,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub code: FailureCode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerPage {
    pub after: Option<WorkerId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScopePage {
    pub after: Option<AppId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManagementStatus {
    pub app_id: AppId,
    pub request_id: RequestId,
}

declare_entity_id! {
    /// An enrolled worker instance named by a placement assignment.
    WorkerId,
    typed_id::WORKER_INSTANCE_PREFIX,
    worker_id_tests,
}
declare_entity_id! {
    /// A customer's workflow run selected by a management command.
    RunId,
    typed_id::WORKFLOW_RUN_PREFIX,
    run_id_tests,
}
declare_entity_id! {
    /// Stable identity for a retried workflow mutation or management command.
    RequestId,
    typed_id::WORKFLOW_REQUEST_PREFIX,
    request_id_tests,
}

/// A positive revision representable by the coordinator's database counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
pub struct Revision(NonZeroI64);
impl Revision {
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0.get()
    }
}
impl TryFrom<i64> for Revision {
    type Error = &'static str;
    fn try_from(value: i64) -> Result<Self, Self::Error> {
        NonZeroI64::new(value)
            .filter(|value| value.get() > 0)
            .map(Self)
            .ok_or("workflow coordination revision must be positive")
    }
}
impl From<Revision> for i64 {
    fn from(value: Revision) -> Self {
        value.get()
    }
}

/// Absolute metadata deadline. Runtime execution uses its own monotonic budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
pub struct UnixMillis(i64);
impl UnixMillis {
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}
impl TryFrom<i64> for UnixMillis {
    type Error = &'static str;
    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if value < 0 {
            Err("workflow coordination timestamp cannot be negative")
        } else {
            Ok(Self(value))
        }
    }
}
impl From<UnixMillis> for i64 {
    fn from(value: UnixMillis) -> Self {
        value.get()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    Ready,
    Draining,
}

/// The registry obtains worker identity from its authenticated service call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegisterWorker {
    /// Concurrent app placements; execution slot capacity stays worker-local.
    pub capacity: NonZeroU32,
    pub state: WorkerState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegisteredWorker {
    pub worker_id: WorkerId,
    pub capacity: NonZeroU32,
    pub state: WorkerState,
    pub expires_at: UnixMillis,
}

/// Control authorizes placement; workers cannot nominate their own app scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AssignScope {
    pub request_id: RequestId,
    pub app_id: AppId,
    pub worker_id: WorkerId,
    /// Absence asserts that this app/worker placement has never existed.
    pub expected_revision: Option<Revision>,
}

/// Placement authority is independent of a customer journal's run/task lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Assignment {
    pub app_id: AppId,
    pub worker_id: WorkerId,
    pub revision: Revision,
    pub expires_at: UnixMillis,
}

/// A worker may renew only its current, unexpired placement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AssignedScope {
    pub app_id: AppId,
    pub assignment_revision: Revision,
}

/// Control checks a worker's current app authority without renewing its lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VerifyAssignment {
    pub app_id: AppId,
    pub worker_id: WorkerId,
    pub assignment_revision: Revision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReleaseScope {
    pub request_id: RequestId,
    pub app_id: AppId,
    pub assignment_revision: Revision,
    /// Confirmed hint after the worker quiesced its local scheduling writes.
    pub wake_revision: Revision,
}

/// Persisted by the worker before publication; a hint cannot claim a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublishWakeHint {
    pub app_id: AppId,
    pub assignment_revision: Revision,
    pub revision: Revision,
    pub next_due_at: Option<UnixMillis>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WakeHintReceipt {
    pub app_id: AppId,
    pub assignment_revision: Revision,
    pub revision: Revision,
}

/// Only lifecycle metadata can enter the durable management queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagementOperation {
    Transition { operation: RunOperation },
    Restart { options: RestartOptions },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManageRun {
    pub request_id: RequestId,
    pub app_id: AppId,
    pub run_id: RunId,
    pub command: ManagementOperation,
}

/// Acknowledgements contain no free-form customer error or output data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagementOutcome {
    Applied { state: RunState },
    // Empty struct variants enforce deny_unknown_fields. Internally tagged
    // unit variants otherwise discard additional fields during deserialization.
    NotFound {},
    Conflict {},
    Denied {},
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AcknowledgeManagement {
    pub request_id: RequestId,
    pub app_id: AppId,
    pub assignment_revision: Revision,
    pub outcome: ManagementOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManagementReceipt {
    pub app_id: AppId,
    pub request_id: RequestId,
    pub outcome: Option<ManagementOutcome>,
}
