//! Native ORM mappings for the Control catalog rows that deploy, archive,
//! restore, publication and deployment collection share.
//!
//! `apps` and `database_bindings` declare only the
//! columns these operations read or write. The deployment catalog models come from
//! `zeroship_workflow_manager::deployments` so retention fences and this
//! catalog agree on one mapping. `tests/deploy_publication/schema.rs` compares
//! every declared field with the migrated platform catalog.

use zeroship_data_orm::schema::Schema;

zeroship_data_orm::orm::schema! {
    pub catalog {
        apps {
            #[orm(primary_key)]
            id: Text,
            deploy_hash: Nullable<Text>,
            manifest_json: Nullable<Text>,
            env_version: BigInt,
            lifecycle_revision: BigInt,
            archived_at: Nullable<Timestamp>,
            deleted_at: Nullable<Timestamp>,
            updated_at: Timestamp,
        }

        database_bindings {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            database_id: Text,
            status: Text,
            generation: Integer,
            observed_generation: Integer,
        }

        // Only the two columns admission reads. A binding is not live unless
        // its DATABASE is live too, and omitting that conjunct admitted a
        // deploy against a database being deleted.
        databases {
            #[orm(primary_key)]
            id: Text,
            status: Text,
        }

        app_deploy_commands {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            actor_id: Nullable<Text>,
            operation: Text,
            content_type: Text,
            archive_sha256: Text,
            deploy_id: Text,
            deploy_hash: Text,
            lifecycle_revision: Nullable<BigInt>,
            result: Text,
            created_at: Timestamp,
        }

        app_lifecycle_intents {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            revision: BigInt,
            action: Text,
            deploy_id: Nullable<Text>,
            registration: Nullable<Text>,
            state: Text,
            receipt: Nullable<Text>,
            created_at: Timestamp,
            acknowledged_at: Nullable<Timestamp>,
        }
    }
}

/// The catalog collections a Control ORM database installs: these models plus
/// the shared deployment and hold models.
///
/// # Errors
/// Rejects invalid native model declarations.
pub fn collections() -> Result<Schema, zeroship_data_orm::error::DbError> {
    let mut collections = zeroship_workflow_manager::deployments::collections()
        .map_err(|_| {
            zeroship_data_orm::error::DbError::config(
                "invalid_deployment_models",
                "deployment catalog models are invalid",
            )
        })?
        .into_collections();
    collections.extend(catalog::schema().into_collections());
    let schema = Schema::new(collections);
    schema.validate()?;
    Ok(schema)
}
