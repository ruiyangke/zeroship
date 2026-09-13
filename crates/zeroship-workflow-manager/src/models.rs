use crate::error::Error;
use zeroship_core::{
    app_id::AppId,
    workflow_jobs::{DeploymentId, JobId, JobSpec},
};
use zeroship_data_orm::{Value, orm::FromRow};

zeroship_data_orm::orm::schema!(pub schema = "../schema/schema.runtime.json");
pub use schema::{jobs, queue_scopes};

/// Canonical metadata for a host's native platform database binding.
///
/// # Errors
/// Refuses incomplete or invalid generated collection metadata.
pub fn collections() -> Result<Vec<(String, Value)>, Error> {
    let descriptor: Value = serde_json::from_str(include_str!("../schema/schema.runtime.json"))
        .map_err(|_| Error::Storage)?;
    descriptor["collections"]
        .as_object()
        .ok_or(Error::Storage)?
        .iter()
        .map(|(name, collection)| {
            Ok((
                name.clone(),
                collection.get("fields").ok_or(Error::Storage)?.clone(),
            ))
        })
        .collect()
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
