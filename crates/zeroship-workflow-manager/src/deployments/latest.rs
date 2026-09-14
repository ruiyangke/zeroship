//! Observe Control's current deployment pointer without acquiring execution authority.

use super::models::app_deploys as deploys;
use crate::Error;
use zeroship_core::{app_id::AppId, workflow_jobs::DeploymentId};
use zeroship_data_orm::{
    orm::{Database, Entity, FromRow},
    schema::Schema,
};

zeroship_data_orm::orm::schema! {
    source {
        apps {
            #[orm(primary_key)]
            id: Text,
            deploy_hash: Nullable<Text>,
        }
    }
}
use source::apps;

/// Native metadata for a host-provisioned Control database binding.
/// The reader selects only deployment identity, hash and retention state.
///
/// # Errors
/// Refuses invalid native model declarations.
pub fn collections() -> Result<Schema, Error> {
    let schema = Schema::new(vec![
        (
            apps::Entity::COLLECTION.into(),
            apps::Entity::schema().clone(),
        ),
        (
            deploys::Entity::COLLECTION.into(),
            deploys::Entity::schema().clone(),
        ),
    ]);
    schema.validate()?;
    Ok(schema)
}

/// A selection observed in one database statement. The app pointer may change
/// afterwards; this value supplies neither a hold nor admission authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestDeployment {
    pub app_id: AppId,
    pub deployment_id: DeploymentId,
    pub deploy_hash: String,
}

#[derive(FromRow)]
#[orm(entity = apps)]
struct AppPointer {
    id: String,
    deploy_hash: Option<String>,
}

#[derive(FromRow)]
#[orm(entity = deploys)]
struct Target {
    id: String,
    app_id: String,
    deploy_hash: String,
    retention_state: String,
}

/// Read-only access to the ordinary app pointer and deployment catalog.
#[derive(Debug, Clone)]
pub struct LatestDeploymentSource {
    database: Database,
}

impl LatestDeploymentSource {
    /// Bind a database provisioned and authorized by the platform host.
    ///
    /// # Errors
    /// Refuses missing or incompatible native collection metadata.
    pub fn new(database: Database) -> Result<Self, Error> {
        database.entity::<apps::Entity>()?;
        database.entity::<deploys::Entity>()?;
        Ok(Self { database })
    }

    /// Observe the exact current target without consulting activation history.
    /// The caller separately obtains policy and retention authority.
    ///
    /// # Errors
    /// Missing pointers or unavailable targets are unavailable. Malformed
    /// stored identities, hashes or retention states are storage failures.
    pub async fn observe(&self, app: &AppId) -> Result<LatestDeployment, Error> {
        let pointer = self
            .database
            .entity::<apps::Entity>()?
            .alias("current_app")?;
        let target = self
            .database
            .entity::<deploys::Entity>()?
            .alias("current_deploy")?;
        let rows = self
            .database
            .from(&pointer)
            .left_join(
                &target,
                pointer
                    .column(apps::id)
                    .eq(target.column(deploys::app_id))?
                    .and(
                        pointer
                            .column(apps::deploy_hash)
                            .eq(target.column(deploys::deploy_hash))?,
                    ),
            )?
            .filter(pointer.column(apps::id).eq(app.as_str())?)
            .select((pointer.row::<AppPointer>(), target.optional_row::<Target>()))?
            .limit(2)?
            .all()
            .await?;
        let (pointer, target) = match rows.as_slice() {
            [] => return Err(Error::Unavailable),
            [row] => row,
            _ => return Err(Error::Storage),
        };
        let app_id = AppId::parse(&pointer.id).map_err(|_| Error::Storage)?;
        if &app_id != app {
            return Err(Error::Storage);
        }
        let hash = pointer.deploy_hash.as_deref().ok_or(Error::Unavailable)?;
        if !zeroship_bundle::validate_hash_format(hash) {
            return Err(Error::Storage);
        }
        let target = target.as_ref().ok_or(Error::Unavailable)?;
        let deployment_id = DeploymentId::parse(&target.id).map_err(|_| Error::Storage)?;
        if AppId::parse(&target.app_id).map_err(|_| Error::Storage)? != app_id
            || target.deploy_hash != hash
            || !zeroship_bundle::validate_hash_format(&target.deploy_hash)
        {
            return Err(Error::Storage);
        }
        match target.retention_state.as_str() {
            "available" => Ok(LatestDeployment {
                app_id,
                deployment_id,
                deploy_hash: target.deploy_hash.clone(),
            }),
            "reclaiming" | "deleted" => Err(Error::Unavailable),
            _ => Err(Error::Storage),
        }
    }
}
