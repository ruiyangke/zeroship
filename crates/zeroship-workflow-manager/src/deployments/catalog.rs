//! Registration in the normal deployment catalog, independent of a customer journal.

use super::Error;
use super::{deploys, invalid_storage, DeploymentHolds};
use std::{future::Future, pin::Pin};
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_data_orm::{
    error::DbError,
    orm::{Entity, FindOptions, FromRow},
    value,
};

#[derive(FromRow)]
#[orm(entity = deploys)]
struct RegisteredDeployment {
    id: String,
    manifest_json: String,
    retention_state: String,
}

impl DeploymentHolds {
    /// Record an ingested normal app deployment and return its stable identity.
    /// The host must publish the verified manifest and blobs before registering.
    /// A repeated or concurrent publication cannot reset retention state.
    ///
    /// # Errors
    /// Rejects invalid manifests and reclamation tombstones, and reports storage
    /// failures. This operation does not acquire a replay hold.
    #[must_use]
    pub fn record_deployment<'a>(
        &'a self,
        app: &'a AppId,
        hash: &'a str,
        manifest_json: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, Error>> + 'a>> {
        // Keep concurrent publishers from embedding the ORM query future's
        // layout into each caller. The catalog remains local to its compio thread.
        Box::pin(async move {
            zeroship_bundle::verify_deployment_manifest(manifest_json.as_bytes(), hash)
                .map_err(|_| Error::InvalidRequest("invalid app deployment manifest".into()))?;
            // Plain insertion lets the unique app/hash index arbitrate concurrent
            // publishers without an upsert overwriting an existing retention fence.
            let inserted = self
                .database
                .collection(deploys::Entity::COLLECTION)?
                .insert(value!({
                    "id":typed_id::generate("dep"),
                    "app_id":app.uuid().to_string(),
                    "deploy_hash":hash,
                    "manifest_json":manifest_json,
                    "activated_at":null
                }))
                .await;
            match inserted {
                Ok(_)
                | Err(
                    DbError::UniqueViolation { .. }
                    | DbError::SchemaRefused {
                        code: "unique_violation",
                        ..
                    },
                ) => {}
                Err(error) => return Err(error.into()),
            }
            let rows = self
                .database
                .entity::<deploys::Entity>()?
                .find::<RegisteredDeployment>(
                    deploys::app_id
                        .eq(app.uuid().to_string())?
                        .and(deploys::deploy_hash.eq(hash)?),
                    FindOptions::default(),
                )
                .await?;
            let [record] = rows.as_slice() else {
                return Err(invalid_storage());
            };
            typed_id::parse_with_prefix(&record.id, "dep").map_err(|_| invalid_storage())?;
            zeroship_bundle::verify_deployment_manifest(record.manifest_json.as_bytes(), hash)
                .map_err(|_| invalid_storage())?;
            match record.retention_state.as_str() {
                "available" => Ok(record.id.clone()),
                "reclaiming" | "deleted" => {
                    Err(Error::Conflict("app deployment is being reclaimed".into()))
                }
                _ => Err(invalid_storage()),
            }
        })
    }
}
