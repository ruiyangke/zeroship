use crate::error::Error;
use zeroship_core::{
    app_id::AppId,
    workflow_jobs::{DeploymentId, JobId, JobSpec},
};
use zeroship_data_orm::{orm::FromRow, schema::Schema};

mod schema_definition;
pub use schema::{
    capacity_targets, jobs, management, management_scopes, queue_scopes, recovery_duties,
    recovery_scopes,
};
pub use schema_definition::schema;

/// Canonical metadata for a host's native platform database binding.
///
/// # Errors
/// Refuses invalid native model declarations.
pub fn collections() -> Result<Schema, Error> {
    let schema = schema::schema();
    schema.validate()?;
    Ok(schema)
}

#[derive(FromRow)]
#[orm(entity = jobs)]
pub struct Job {
    pub id: String,
    pub app_id: String,
    pub deployment_id: Option<String>,
    pub operation: String,
    pub operation_kind: String,
    pub management_request_id: Option<String>,
    pub run_id: Option<String>,
    pub spec_digest: String,
    pub available_at: i64,
    pub dispatch_order: i64,
    pub state: String,
    pub attempt: i64,
    /// Attempts a worker confirmed had begun executing, by renewing their lease.
    pub execution_attempts: i64,
    /// The latest attempt counted into `execution_attempts`, so repeated
    /// renewals of one delivery contribute once.
    pub executed_attempt: Option<i64>,
    pub worker_id: Option<String>,
    pub lease_deadline: Option<i64>,
    pub leased_at: Option<i64>,
    pub deferrals: i64,
    pub outcome: Option<String>,
    pub settlement_digest: Option<String>,
    pub created_at: i64,
}

impl Job {
    pub fn spec(&self) -> Result<JobSpec, Error> {
        let spec = JobSpec {
            id: JobId::parse(&self.id).map_err(|_| Error::Storage)?,
            app_id: AppId::parse(&self.app_id).map_err(|_| Error::Storage)?,
            operation: serde_json::from_str(&self.operation).map_err(|_| Error::Storage)?,
            available_at: self.available_at.try_into().map_err(|_| Error::Storage)?,
        };
        if self.deployment_id.as_deref() != spec.deployment_id().map(DeploymentId::as_str)
            || self.management_request_id.as_deref() != management_request(&spec.operation)
            || self.operation_kind != operation_kind(&spec.operation)
            || self.run_id.as_deref() != operation_run(&spec.operation)
        {
            return Err(Error::Storage);
        }
        let encoded = serde_json::to_vec(&spec).map_err(|_| Error::Storage)?;
        if crate::queue::digest(&encoded) != self.spec_digest {
            return Err(Error::Storage);
        }
        Ok(spec)
    }
}

#[derive(FromRow)]
#[orm(entity = queue_scopes)]
pub struct Scope {
    #[expect(
        dead_code,
        reason = "queue_scopes.id completes the FromRow projection; only dispatch_cursor is read"
    )]
    pub id: String,
    pub execution_zone_id: String,
    pub dispatch_cursor: i64,
}

#[derive(FromRow)]
#[orm(entity = management)]
pub struct Management {
    pub id: String,
    pub app_id: String,
    pub request_id: String,
    pub run_id: String,
    pub revision: i64,
    pub actor: String,
    pub request: String,
    pub request_digest: String,
    pub blocks_execution: bool,
    pub created_at: i64,
    pub outcome: Option<String>,
}

#[derive(FromRow)]
#[orm(entity = management_scopes)]
pub struct ManagementScope {
    pub id: String,
    pub app_id: String,
    pub run_id: String,
    pub accepted_revision: i64,
    pub settled_revision: i64,
}

/// What running a queued row does, which decides who may claim it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Work {
    /// A sweep of the journal. It runs no creator code. Some of these sweeps
    /// also move payload objects, which does not separate them: the process that
    /// owns the journal owns the store those objects live in, so a host that can
    /// run one of these can run all of them.
    Maintenance,
    /// Creator code.
    Creator,
}

/// A host that takes rows off an app's queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claimant {
    /// An enrolled worker in the app's execution zone. It runs creator code,
    /// while maintenance stays with the journal-owning service.
    Worker,
    /// A lane in the process that owns the journal and the payload store, so
    /// every sweep is its to take and creator code is not.
    Maintenance,
}

impl Claimant {
    /// The kinds this claimant refuses, for a predicate over
    /// `jobs.operation_kind`. Every claimant refuses some kind, because each
    /// `Work` class admits exactly one of them.
    pub fn denied(self) -> impl Iterator<Item = &'static str> {
        OPERATIONS
            .iter()
            .filter(move |(_, work)| !self.admits(*work))
            .map(|(kind, _)| *kind)
    }

    /// Every pairing is named, with no wildcard on either side: a `Work` class
    /// or a claimant added to this file leaves this match non-exhaustive, so
    /// whether that host takes that work is decided here rather than inherited
    /// from an arm that happened to cover it.
    ///
    /// Each `Work` class has exactly one claimant here, so which host runs a
    /// kind is a compiler-checked property of this match rather than whichever
    /// host asked first.
    const fn admits(self, work: Work) -> bool {
        match (self, work) {
            (Self::Worker, Work::Creator) | (Self::Maintenance, Work::Maintenance) => true,
            (Self::Worker, Work::Maintenance) | (Self::Maintenance, Work::Creator) => false,
        }
    }
}

/// The `jobs.operation_kind` column's vocabulary, declared once.
///
/// One arm per `JobOperation` variant produces both the match that writes the
/// column and the table a claim predicate reads, so the two cannot disagree: a
/// variant added to `JobOperation` leaves this match non-exhaustive, and the
/// arm that repairs it has to name the work the row carries before any
/// claimant can be told whether to take it.
macro_rules! operations {
    ($($variant:ident => $kind:literal, $work:ident;)+) => {
        pub const fn operation_kind(
            operation: &zeroship_core::workflow_jobs::JobOperation,
        ) -> &'static str {
            use zeroship_core::workflow_jobs::JobOperation;
            match operation {
                $(JobOperation::$variant { .. } => $kind,)+
            }
        }

        const OPERATIONS: &[(&str, Work)] = &[$(($kind, Work::$work),)+];
    };
}

operations! {
    Activate => "activate", Maintenance;
    Advance => "advance", Creator;
    Cron => "cron", Maintenance;
    Management => "management", Maintenance;
    Fanout => "fanout", Maintenance;
    Propagate => "propagate", Maintenance;
    ReleaseHold => "release_hold", Maintenance;
    Close => "close", Maintenance;
    Reconcile => "reconcile", Maintenance;
    Collect => "collect", Maintenance;
}

pub fn operation_run(operation: &zeroship_core::workflow_jobs::JobOperation) -> Option<&str> {
    use zeroship_core::workflow_jobs::JobOperation;
    match operation {
        JobOperation::Advance { run_id, .. }
        | JobOperation::Cron { run_id, .. }
        | JobOperation::Management { run_id, .. } => Some(run_id.as_str()),
        _ => None,
    }
}

pub fn management_request(operation: &zeroship_core::workflow_jobs::JobOperation) -> Option<&str> {
    match operation {
        zeroship_core::workflow_jobs::JobOperation::Management { request_id, .. } => {
            Some(request_id.as_str())
        }
        _ => None,
    }
}
