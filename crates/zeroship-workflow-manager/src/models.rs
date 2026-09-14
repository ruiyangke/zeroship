use crate::error::Error;
use zeroship_core::{
    app_id::AppId,
    workflow_jobs::{DeploymentId, JobId, JobSpec},
};
use zeroship_data_orm::{orm::FromRow, schema::Schema};

mod schema_definition;
pub use schema::{
    assignments, jobs, management, management_scopes, placement_receipts, queue_scopes,
    recovery_duties, recovery_scopes, workers,
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
    pub worker_id: Option<String>,
    pub assignment_revision: Option<i64>,
    pub lease_deadline: Option<i64>,
    pub outcome: Option<String>,
    pub settlement_digest: Option<String>,
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
#[orm(entity = workers)]
pub struct Worker {
    pub id: String,
    pub capacity: i64,
    pub state: String,
    pub expires_at: i64,
}

#[derive(FromRow)]
#[orm(entity = queue_scopes)]
pub struct Scope {
    pub id: String,
    pub dispatch_cursor: i64,
}

#[derive(FromRow)]
#[orm(entity = assignments)]
pub struct Placement {
    pub id: String,
    pub app_id: String,
    pub worker_id: String,
    pub revision: i64,
    pub expires_at: i64,
    pub released: bool,
    pub wake_revision: Option<i64>,
    pub next_due_at: Option<i64>,
}

#[derive(FromRow)]
#[orm(entity = placement_receipts)]
pub struct PlacementReceipt {
    pub operation: String,
    pub worker_id: String,
    pub expected_revision: Option<i64>,
    pub wake_revision: Option<i64>,
    pub result_revision: i64,
    pub result_expires_at: i64,
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

pub const fn operation_kind(
    operation: &zeroship_core::workflow_jobs::JobOperation,
) -> &'static str {
    use zeroship_core::workflow_jobs::JobOperation;
    match operation {
        JobOperation::Activate { .. } => "activate",
        JobOperation::Advance { .. } => "advance",
        JobOperation::Cron { .. } => "cron",
        JobOperation::Management { .. } => "management",
        JobOperation::Fanout { .. } => "fanout",
        JobOperation::Propagate { .. } => "propagate",
        JobOperation::Reconcile {} => "reconcile",
        JobOperation::Collect {} => "collect",
    }
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
