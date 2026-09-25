//! Metadata exchanged with the workflow coordinator.
//!
//! Execution inputs, journal records and customer connection information
//! belong to customer-worker contracts, not this protocol. A payload's BYTES
//! are the same: what crosses here is at most the descriptor that locates one,
//! as `RunStatus::output` carries, and reading it is a separate exchange with
//! its own budget. Service authentication establishes the caller; IDs in
//! messages select resources and never grant authority to them.

pub use zeroship_id::workflow::{DeploymentId, RequestId, RunId, WorkerId};

mod lifecycle;
pub use lifecycle::{
    ConflictPolicy, DeliveredSignal, InvalidRestart, RestartDeploy, RestartOptions, RestartTarget,
    RestartedRun, RunOperation, RunState, RunStatus, SignalOptions, StartOptions, StartedRun,
    TransitionedRun, WorkflowOutputRef,
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

/// Decode a field that may be null but may never be absent.
///
/// A bare `Option` field reads a missing key as `None`, so a producer that
/// dropped the key would read back as a different valid message rather than
/// fail: a whole restart instead of a partial one, the first page instead of
/// the one the scan asked for, an unapplied command instead of a settled one.
/// Naming a `deserialize_with` makes serde raise `missing_field` for the
/// absent key and keeps `null` a value the field still carries.
fn nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScopePage {
    /// The last app the previous page returned, null to scan from the first.
    #[serde(deserialize_with = "nullable")]
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

/// The deployment a latest restart replays against, named by the authority for
/// it rather than resolved by the manager.
///
/// This is a Control-only fact, which is why it sits on [`ManagementOperation`]
/// and not on [`RestartOptions`]: those options reach the manager from creator
/// code through the workflow service's run binding, and creator code may not
/// choose the deployment its run restarts onto. A `ManagementOperation` is
/// reachable only through [`ManageRun`], whose endpoint only Control may call.
///
/// The hash travels with the id because the manager checks the pair against the
/// hold Control minted for that deployment. An id alone would name a deployment
/// without saying which code it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestartDeployment {
    pub deployment_id: DeploymentId,
    pub deploy_hash: String,
}

/// Only lifecycle metadata can enter the durable management queue.
///
/// `deployment` is present exactly when the restart's effective deploy policy
/// is [`RestartDeploy::Latest`], and absent otherwise. The manager's
/// `validate_request` refuses both mismatches, so the two spellings of one
/// decision cannot drift apart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagementOperation {
    Transition {
        operation: RunOperation,
    },
    Restart {
        options: RestartOptions,
        #[serde(deserialize_with = "nullable")]
        deployment: Option<RestartDeployment>,
    },
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
    /// The command's result, null while the manager has accepted it and not
    /// yet applied it.
    #[serde(deserialize_with = "nullable")]
    pub outcome: Option<ManagementOutcome>,
}

/// Selects one run of one app for a creator-facing call.
///
/// The worker is absent on purpose. Placement authority is `(app, worker)`, and
/// the worker half comes from the credential that verified the request, so a
/// body cannot name a placement its caller does not hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunScope {
    pub scope: AssignedScope,
    pub run_id: RunId,
}

/// Deliver a signal to a waiting run.
///
/// `request_id` is the idempotency of the delivery: a caller that retries after
/// an uncertain reply sends the same one rather than delivering twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignalRun {
    pub request_id: RequestId,
    pub scope: AssignedScope,
    pub run_id: RunId,
    pub options: SignalOptions,
}

/// Move a run through a lifecycle transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TransitionRun {
    pub request_id: RequestId,
    pub scope: AssignedScope,
    pub run_id: RunId,
    pub operation: RunOperation,
}

/// Restart a run, retaining whatever journal prefix the options name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestartRun {
    pub request_id: RequestId,
    pub scope: AssignedScope,
    pub run_id: RunId,
    pub options: RestartOptions,
}

/// Why a creator-facing run call was refused, in the engine's own terms.
///
/// [`Failure`] cannot carry this. It has seven codes, no arm for a missing run,
/// and no message at all, while creator code branches on the code AND reads the
/// message: `invalid_request_keeps_its_message` and `conflict_keeps_its_message`
/// in `crates/zeroship-workflow-v8/src/error.rs` pin that contract.
///
/// TWO ARMS CARRY NO MESSAGE FIELD, and that is the contract rather than an
/// omission. `Internal` and `Unavailable` name a host condition a creator cannot
/// act on, so their wording is replaced before it reaches creator code while the
/// operator reads the original in the service log. Giving them nowhere to put a
/// message means host wording cannot cross even by mistake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "code",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum RunFailure {
    InvalidRequest { message: String },
    Unauthenticated {},
    PermissionDenied {},
    NotFound { message: String },
    Conflict { message: String },
    ResourceExhausted { message: String },
    PayloadTooLarge {},
    Unavailable {},
    Timeout {},
    /// Retry after establishing an epoch above `after`; null means none was
    /// held and the journal has closed none. Never a durable refusal.
    IngressFenced {
        #[serde(deserialize_with = "nullable")]
        after: Option<Revision>,
    },
    Internal {},
}

impl RunFailure {
    /// The HTTP status this refusal is carried by.
    ///
    /// One authority for the pairing, so the service that writes the status and
    /// the client that checks it cannot disagree. A reply whose status and body
    /// disagree is not a refusal this contract describes, and the client refuses
    /// it rather than believing either half.
    #[must_use]
    pub const fn status(&self) -> u16 {
        match self {
            Self::InvalidRequest { .. } => 400,
            Self::Unauthenticated {} => 401,
            Self::PermissionDenied {} => 403,
            Self::NotFound { .. } => 404,
            Self::Conflict { .. } => 409,
            Self::IngressFenced { .. } => 412,
            Self::PayloadTooLarge {} => 413,
            Self::ResourceExhausted { .. } => 429,
            Self::Internal {} => 500,
            Self::Unavailable {} => 503,
            Self::Timeout {} => 504,
        }
    }
}
