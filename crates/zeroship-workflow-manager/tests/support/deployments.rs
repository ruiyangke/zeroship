use crate::support::{Admin, Fixture};
use zeroship_core::{
    app_id::AppId, schema_name::SchemaName, workflow_coordination::RestartDeployment,
};
use zeroship_data_orm::{
    binding::DbBinding,
    encryption::ProjectKeySource,
    orm::{Database, Operation, Output},
    value, ConnectOptions, Value,
};
use zeroship_workflow_manager::deployments::{self, DeploymentHolds};

/// Control's deployment catalog and hold ledger, which the manager reaches only
/// through the hold client. Nothing here stands in for an app pointer: the
/// deployment a management command names arrives on the wire from Control.
pub struct Source {
    pub database: Database,
    pub ledger: DeploymentHolds,
}

impl Source {
    pub async fn new(fixture: &Fixture) -> Self {
        let (namespace, writer_url) = match &fixture.admin {
            Admin::Postgres(admin) => {
                admin
                    .batch_execute("CREATE SCHEMA zeroship;")
                    .await
                    .unwrap();
                admin
                    .batch_execute(deployments::POSTGRES_SCHEMA)
                    .await
                    .unwrap();
                (
                    "zeroship",
                    fixture.url().replace("workflow_manager_test@", "postgres@"),
                )
            }
            Admin::Sqlite(admin) => {
                admin.execute_batch(deployments::SQLITE_SCHEMA).unwrap();
                ("main", fixture.url().to_owned())
            }
        };
        let binding = DbBinding::platform(
            "platform",
            "management-selection",
            SchemaName::new(namespace).unwrap(),
        );
        let database = Database::connect(
            binding,
            ConnectOptions::new(writer_url, ProjectKeySource::unavailable()).connection_authority(),
            deployments::collections().unwrap(),
        )
        .await
        .unwrap();
        Self {
            ledger: DeploymentHolds::new(database.clone()).unwrap(),
            database,
        }
    }

    /// Record a deployment in Control's catalog and name it the way Control
    /// would on the wire.
    pub async fn publish(&self, app: &AppId, marker: &str) -> RestartDeployment {
        let mut manifest = serde_json::to_value(zeroship_bundle::Manifest::default()).unwrap();
        manifest["fixture_marker"] = serde_json::json!(marker);
        let hash =
            zeroship_bundle::deployment_manifest_hash(&serde_json::to_vec(&manifest).unwrap())
                .unwrap();
        manifest["deploy_hash"] = serde_json::json!(hash);
        let id = self
            .ledger
            .record_deployment(app, &hash, &serde_json::to_string(&manifest).unwrap())
            .await
            .unwrap();
        RestartDeployment {
            deployment_id: zeroship_core::workflow_jobs::DeploymentId::parse(&id).unwrap(),
            deploy_hash: hash,
        }
    }

    pub async fn patch(&self, table: &str, id: &str, patch: Value) {
        let output = self
            .database
            .collection(table)
            .unwrap()
            .execute(Operation::Update {
                filter: value!({"id":id}),
                patch,
                many: true,
            })
            .await
            .unwrap();
        assert!(matches!(output, Output::Count(1)));
    }
}
