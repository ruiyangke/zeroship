//! Closed metadata for durable workflow delivery across database zones.
//!
//! Customer inputs, history and outputs stay in creator storage. The manager
//! validates app scope, execution authority and successor bounds separately.

pub use zeroship_id::workflow::{BroadcastId, DeploymentId, JobId, PropagationId};

use crate::{
    app_id::AppId,
    workflow_coordination::{
        AssignedScope, ManagementOutcome, RequestId, RestartTarget, Revision, RunId, RunOperation,
        UnixMillis, WorkerId,
    },
    workflow_schedules::ScheduleId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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
    /// One bounded page of a creator dependency propagation obligation. Its
    /// kind, source run and cursor stay in the creator journal.
    Propagate {
        propagation_id: PropagationId,
        revision: Revision,
    },
    /// Ask the creator engine to give back the journal hold on a deployment the
    /// app no longer selects. Only a manager publishes it; a worker cannot. The
    /// named deployment is the release target, never an executable prerequisite,
    /// so this operation stays deliverable without a queue hold.
    ReleaseHold {
        deployment_id: DeploymentId,
    },
    /// Manager-origin closure of one ingress epoch. The creator raises its
    /// closed epoch and reports closed drain evidence in the same transaction.
    /// Workers can neither publish it nor name it as a successor.
    Close {
        epoch: Revision,
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
            | JobOperation::Propagate { .. }
            | JobOperation::ReleaseHold { .. }
            | JobOperation::Close { .. }
            | JobOperation::Reconcile {}
            | JobOperation::Collect {} => None,
        }
    }

    /// The deployment a release operation targets. It is never a prerequisite,
    /// so `deployment_id` keeps returning `None` for it: a release is published
    /// after the queue holder gave the deployment back, when no hold remains to
    /// confirm.
    #[must_use]
    pub const fn released_deployment(&self) -> Option<&DeploymentId> {
        match &self.operation {
            JobOperation::ReleaseHold { deployment_id } => Some(deployment_id),
            _ => None,
        }
    }

    /// Whether executing this job can commit new creator intents. Reconciliation
    /// and collection only publish or delete existing records, closure only
    /// reports evidence, and a release only gives a deployment back, so none of
    /// them can re-establish recovery responsibility.
    #[must_use]
    pub const fn produces_intents(&self) -> bool {
        match self.operation {
            JobOperation::Activate { .. }
            | JobOperation::Advance { .. }
            | JobOperation::Cron { .. }
            | JobOperation::Management { .. }
            | JobOperation::Fanout { .. }
            | JobOperation::Propagate { .. } => true,
            JobOperation::ReleaseHold { .. }
            | JobOperation::Close { .. }
            | JobOperation::Reconcile {}
            | JobOperation::Collect {} => false,
        }
    }

    /// The id this specification's own content derives, when the operation is
    /// one a creator journal publishes.
    ///
    /// Equal to `self.id` for every publication intent the journal holds, and
    /// that equality is the integrity check a reader applies: the key carries
    /// the identity, so a specification that no longer derives the key it is
    /// stored under is a damaged journal rather than a different job.
    #[must_use]
    pub fn publication_id(&self) -> Option<JobId> {
        publication_id(&self.app_id, &self.operation, self.available_at)
    }
}

/// The primary key of a publication intent, derived from the work it names.
///
/// `None` for the seven operations a creator journal never publishes, which is
/// how an intent row wearing one of them is refused.
///
/// # What each kind's identity is, and why
///
/// The derivation covers the operation in full, and additionally covers the due
/// time for `Advance` alone:
///
/// - **`Advance`** is identified by its deployment, run, generation, frontier
///   revision AND due time. A run whose frontier revision has not moved but
///   whose due time has is a different job, so rescheduling a frontier produces
///   a different id rather than silently rewriting a published job's due time.
/// - **`Fanout`** is identified by its broadcast and page revision, and
///   **`Propagate`** by its obligation and page revision. Neither carries the
///   due time: both are recorded as available now, so including it would make
///   every observation of the same page a different job and there would be no
///   deduplication at all.
///
/// # The shape of the input
///
/// Every field is preceded by a one-byte type tag and, for text, its length, so
/// no concatenation of one field's value can be read as another's. The leading
/// domain and kind tags keep two kinds from meeting on equal-looking inputs.
#[must_use]
pub fn publication_id(
    app: &AppId,
    operation: &JobOperation,
    available_at: UnixMillis,
) -> Option<JobId> {
    let mut identity = match operation {
        JobOperation::Advance { .. } => Identity::new("advance"),
        JobOperation::Fanout { .. } => Identity::new("fanout"),
        JobOperation::Propagate { .. } => Identity::new("propagate"),
        JobOperation::Activate { .. }
        | JobOperation::Cron { .. }
        | JobOperation::Management { .. }
        | JobOperation::ReleaseHold { .. }
        | JobOperation::Close { .. }
        | JobOperation::Reconcile {}
        | JobOperation::Collect {} => return None,
    };
    identity.text(app.as_str());
    match operation {
        JobOperation::Advance {
            deployment_id,
            run_id,
            generation,
            revision,
        } => {
            identity.text(deployment_id.as_str());
            identity.text(run_id.as_str());
            identity.number(i64::from(*generation));
            identity.number(revision.get());
            identity.number(available_at.get());
        }
        JobOperation::Fanout {
            broadcast_id,
            revision,
        } => {
            identity.text(broadcast_id.as_str());
            identity.number(revision.get());
        }
        JobOperation::Propagate {
            propagation_id,
            revision,
        } => {
            identity.text(propagation_id.as_str());
            identity.number(revision.get());
        }
        JobOperation::Activate { .. }
        | JobOperation::Cron { .. }
        | JobOperation::Management { .. }
        | JobOperation::ReleaseHold { .. }
        | JobOperation::Close { .. }
        | JobOperation::Reconcile {}
        | JobOperation::Collect {} => return None,
    }
    Some(identity.finish())
}

/// The unambiguous encoding [`publication_id`] hashes.
struct Identity(Sha256);

impl Identity {
    /// The domain separator. It is versioned because changing which fields a
    /// kind's identity covers changes every id that kind derives, and two
    /// runtimes disagreeing about that would publish the same work twice.
    const DOMAIN: &'static [u8] = b"zeroship/workflow/publication/1";

    fn new(kind: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(Self::DOMAIN);
        let mut identity = Self(hasher);
        identity.text(kind);
        identity
    }

    fn text(&mut self, value: &str) {
        self.0.update([b't']);
        self.0
            .update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
        self.0.update(value.as_bytes());
    }

    fn number(&mut self, value: i64) {
        self.0.update([b'n']);
        self.0.update(value.to_be_bytes());
    }

    fn finish(self) -> JobId {
        let digest = self.0.finalize();
        let mut body = [0u8; 16];
        body.copy_from_slice(&digest[..16]);
        JobId::derived(body)
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
/// finishes the broadcast expansion. For Propagate, `Waiting` confirms a
/// committed successor page and `Completed` discharges the obligation; neither
/// asserts that affected runs stopped. For reconciliation and collection, `Waiting`
/// requests another scan page or phase and `Completed` closes that scan cycle.
/// Neither classification asserts that the manager queue or app intents drained.
/// For a hold release, `Completed` reports that the journal holder gave the
/// deployment back and `Waiting` that the journal still depends on it, so a
/// later release may succeed. A release is never forced.
/// Only `Closed` reports drain evidence, and only for the closure job's epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum JobOutcome {
    Completed {},
    Waiting {},
    Rejected {},
    Management {
        outcome: ManagementOutcome,
    },
    /// The creator fenced the job's epoch; `drained` reports whether its closed
    /// drain predicates held in that same transaction.
    Closed {
        drained: bool,
    },
}

impl JobOutcome {
    /// Match the outcome family to its operation. Creator handlers separately
    /// enforce their lifecycle rules; this check grants no execution authority.
    #[must_use]
    pub const fn valid_for(&self, operation: &JobOperation) -> bool {
        match (self, operation) {
            (Self::Management { .. }, JobOperation::Management { .. })
            | (Self::Closed { .. }, JobOperation::Close { .. }) => true,
            (Self::Management { .. } | Self::Closed { .. }, _)
            | (_, JobOperation::Management { .. } | JobOperation::Close { .. }) => false,
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
