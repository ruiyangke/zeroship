use crate::error::Error;
use zeroship_core::{
    app_id::AppId,
    workflow_jobs::{DeploymentId, JobId, JobSpec},
};
use zeroship_data_orm::{orm::FromRow, schema::Schema};

mod schema_definition;
pub use schema::{
    assignments, jobs, management, placement_receipts, queue_scopes, recovery_scopes, workers,
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
    pub deployment_id: String,
    pub operation: String,
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
        Ok(JobSpec {
            id: JobId::parse(&self.id).map_err(|_| Error::Storage)?,
            app_id: AppId::parse(&self.app_id).map_err(|_| Error::Storage)?,
            deployment_id: DeploymentId::parse(&self.deployment_id).map_err(|_| Error::Storage)?,
            operation: serde_json::from_str(&self.operation).map_err(|_| Error::Storage)?,
            available_at: self.available_at.try_into().map_err(|_| Error::Storage)?,
        })
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
    pub app_id: String,
    pub request_id: String,
    pub run_id: String,
    pub actor: String,
    pub operation: String,
    pub restart_name: Option<String>,
    pub restart_occurrence: Option<i64>,
    pub restart_deploy: Option<String>,
    pub outcome: Option<String>,
    pub run_state: Option<String>,
}
