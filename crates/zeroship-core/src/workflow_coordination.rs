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
    ConflictPolicy, CreatorStartOptions, DeliveredSignal, InvalidRestart, InvalidStart,
    PayloadLocation, RestartDeploy, RestartOptions, RestartTarget, RestartedRun, RunOperation,
    RunState, RunStatus, SignalOptions, StartOptions, StartedRun, StepOutputLocation,
    TransitionedRun, WorkflowOutputRef,
};

use crate::app_id::AppId;
use serde::{Deserialize, Serialize};
use std::num::NonZeroI64;

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
/// The worker is absent on purpose. The credential that verified the request
/// supplies the worker's zone, which the service compares against the app's
/// frozen zone; a body cannot name a zone its caller does not hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunScope {
    pub app_id: AppId,
    pub run_id: RunId,
}

/// Start a run of one workflow of one app, from the value the caller supplied.
///
/// `request_id` is minted by the CALLER rather than by the service, because it is
/// the idempotency of the whole start: the object the value is staged into is
/// keyed by it too, so a retried start restages to the same object and replays
/// the same receipt.
///
/// NO DEPLOYMENT CROSSES. The service resolves a run's deploy from its own rows,
/// so naming one here would let a body select the code a run replays against.
///
/// `input` is the creator's own value and answers to `AppPolicy::max_input_bytes`,
/// the same bound `SignalRun::options` answers to. `options` is a
/// [`CreatorStartOptions`], which has no field for a payload descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StartRun {
    pub request_id: RequestId,
    pub app_id: AppId,
    pub workflow_name: String,
    #[serde(default)]
    pub input: serde_json::Value,
    #[serde(default)]
    pub options: CreatorStartOptions,
}

/// Locate what one completed step of a run recorded.
///
/// The reply is a [`StepOutputLocation`], never the bytes: a payload's ceiling is
/// larger than a reply's, and this transport has no byte-stream path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadStepOutput {
    pub app_id: AppId,
    pub run_id: RunId,
    pub name: String,
    pub occurrence: u32,
}

/// Locate the object one replay edge of a running task names.
///
/// The reply is a [`PayloadLocation`], for the reason [`ReadStepOutput`]'s is:
/// the bytes answer to a payload's ceiling and a reply to a smaller one. The
/// caller opens the object itself, out of the lock this call takes and releases.
///
/// THE APP IS A SELECTOR AND NOT THE AUTHORITY. It names which journal to ask,
/// and the authority is the task credential, which that journal minted and holds
/// only a hash of. A caller naming another app's journal reaches one where its
/// own task and token do not exist, so the selector cannot widen what it may
/// read.
///
/// `token` IS A STRING because the token's type belongs to the journal crate,
/// which this one cannot name. The wire bytes are the same either way: that type
/// serializes as a string already. Parsing it on the CALLEE's side is the point,
/// since refusing a malformed credential is the callee's to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadTaskPayload {
    pub app_id: AppId,
    pub task_id: String,
    pub token: String,
    pub reference: WorkflowOutputRef,
}

/// Resolve which deployment a live dispatch replays against.
///
/// THE ARTIFACT DOES NOT CROSS, and here that is structural rather than a
/// budget choice. A loaded executable carries the creator's module source, whose
/// own budget is twice the ceiling a reply answers to, so a deployment at its
/// permitted size could never fit one. What crosses is the PIN: the caller loads
/// that deployment from the object store it already holds, which is addressed by
/// the deploy hash and therefore answers with the same immutable bytes either
/// host would read.
///
/// Selector and authority split exactly as [`ReadTaskPayload`]'s do: the app
/// names the journal to ask, the task credential is what authorizes the answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResolveTaskExecutable {
    pub app_id: AppId,
    pub task_id: String,
    pub token: String,
}

/// The deployment a run is pinned to, as the journal proved it.
///
/// Four fields, and only the first two address the artifact. The other two are
/// the FENCES the journal checked, echoed back so the settlement that reports an
/// execution can be refused when either has moved underneath it: a deployment
/// parked for damage bumps its availability epoch, and re-admission moves the
/// admission generation. A caller does not interpret them and cannot forge a
/// useful one -- they are only ever compared against the journal's own rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PinnedDeployment {
    pub deploy_id: DeploymentId,
    pub deploy_hash: String,
    pub availability_epoch: i64,
    pub admission_generation: i64,
}

/// Reserve the row an upload's object will be keyed by.
///
/// THE BYTES DO NOT CROSS HERE and never will: this mints or finds a payload id,
/// and the caller writes the object to the store it already binds. Splitting it
/// this way is what lets a caller upload at all -- in one process the staging
/// call holds a lock across the object write so collection cannot race a live
/// writer, and no request boundary can hold that lock.
///
/// `request_id` IS THE IDEMPOTENCY, and the obligation it places on the caller is
/// real: a retry must send the SAME one. The service deduplicates on
/// `(app_id, request_id)`, narrowed by the task only when one holds the staging,
/// and refuses a request id reused for different bytes. A caller that minted a
/// fresh id per attempt would reserve a new object every time and nothing would
/// fail -- the unique index cannot catch it, because it covers `task_id` and that
/// column is NULL for every ownerless upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReservePayload {
    pub app_id: AppId,
    pub task_id: String,
    pub token: String,
    pub request_id: RequestId,
    pub reference: WorkflowOutputRef,
}

/// What a reservation decided.
///
/// Two arms, and neither is a failure: one says write the object, the other says
/// an earlier attempt already did. A retry after a lost acknowledgement is the
/// ordinary case rather than an edge, so the caller must handle both as success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum PayloadReservation {
    /// Write the object, then confirm. `expires_at` is the deadline the
    /// reservation was recorded with and must be sent back UNCHANGED: the confirm
    /// compares against this value, and one recomputed later can drift past the
    /// collector's fence and compare two different things while still looking
    /// like a comparison.
    Reserved {
        payload_id: String,
        expires_at: i64,
    },
    /// Already uploaded and confirmed. There is no object to write.
    Staged { payload_id: String },
}

/// Deliver a signal to a waiting run.
///
/// `request_id` is the idempotency of the delivery: a caller that retries after
/// an uncertain reply sends the same one rather than delivering twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignalRun {
    pub request_id: RequestId,
    pub app_id: AppId,
    pub run_id: RunId,
    pub options: SignalOptions,
}

/// Move a run through a lifecycle transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TransitionRun {
    pub request_id: RequestId,
    pub app_id: AppId,
    pub run_id: RunId,
    pub operation: RunOperation,
}

/// Restart a run, retaining whatever journal prefix the options name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestartRun {
    pub request_id: RequestId,
    pub app_id: AppId,
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
