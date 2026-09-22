//! Metadata exchanged with the workflow coordinator.
//!
//! Execution inputs, journal records, customer connection information and
//! payload descriptors belong to customer-worker contracts, not this protocol.
//! Service authentication establishes the caller; IDs in messages select
//! resources and never grant authority to them.

pub use zeroship_id::workflow::{DeploymentId, RequestId, RunId, WorkerId};

mod lifecycle;
pub use lifecycle::{
    InvalidRestart, RestartDeploy, RestartOptions, RestartTarget, RunOperation, RunState,
};

use crate::app_id::AppId;
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
pub struct ScopePage {
    pub after: Option<AppId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManagementStatus {
    pub app_id: AppId,
    pub request_id: RequestId,
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

/// Select a worker's current app authority without carrying or renewing its lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VerifyAssignment {
    pub app_id: AppId,
    pub worker_id: WorkerId,
    pub assignment_revision: Revision,
}

impl From<&Assignment> for VerifyAssignment {
    fn from(assignment: &Assignment) -> Self {
        Self {
            app_id: assignment.app_id.clone(),
            worker_id: assignment.worker_id.clone(),
            assignment_revision: assignment.revision,
        }
    }
}

/// A worker gives up one of its own placements. Releasing never discharges
/// the manager's recovery responsibility for the app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReleaseScope {
    pub request_id: RequestId,
    pub app_id: AppId,
    pub assignment_revision: Revision,
    pub reason: ReleaseReason,
}

/// Why a worker released a placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseReason {
    /// The worker no longer serves the app, for example while draining.
    Relinquished,
    /// The worker's resource provider permanently denies this app. The manager
    /// does not offer the app to this worker instance again.
    Refused,
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

/// Decode a field that may be null but may never be absent.
///
/// A bare `Option` field reads a missing key as `None`, so a producer that
/// dropped the retained prefix would read back as a whole restart rather than
/// fail. Naming a `deserialize_with` makes serde raise `missing_field` for the
/// absent key and keeps `null` a value the field still carries.
fn nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

/// Acknowledgements contain no free-form customer error or output data.
///
/// The applied arms are command-shaped: `Applied` answers a transition, whose
/// whole result is the state the run settled in, and `Restarted` answers a
/// restart, which additionally decides how much journal the new generation
/// keeps and where it replays. Both are decided inside the creator transaction,
/// so a caller cannot recover either from the command it sent. The refusal arms
/// are shared, because a refused command is refused the same way whichever of
/// the two it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ManagementOutcome {
    Applied {
        state: RunState,
    },
    /// `pinned_to` is the deployment the restarted generation replays against,
    /// which a latest restart moves; `restarted_from_ordinal` is the retained
    /// journal prefix, null when the run restarted whole.
    Restarted {
        state: RunState,
        #[serde(deserialize_with = "nullable")]
        restarted_from_ordinal: Option<u32>,
        pinned_to: DeploymentId,
    },
    // Empty struct variants enforce deny_unknown_fields. Internally tagged
    // unit variants otherwise discard additional fields during deserialization.
    NotFound {},
    Conflict {},
    Denied {},
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManagementReceipt {
    pub app_id: AppId,
    pub request_id: RequestId,
    pub outcome: Option<ManagementOutcome>,
}
