use crate::support::{Admin, Fixture};
use std::{collections::HashMap, rc::Rc};
use zeroship_bundle::{BlobStore, LocalDiskBlobStore, Manifest, WorkerCode};
use zeroship_core::{
    app_id::AppId, schema_name::SchemaName, workflow_jobs::DeploymentId,
    workflow_schedules::ScheduleDescriptor,
};
use zeroship_data_orm::{
    binding::DbBinding, encryption::ProjectKeySource, orm::Database, ConnectOptions,
};
use zeroship_workflow_manager::{
    deployments::{self, DeploymentHolds},
    retention::{CatalogClient, HoldClient},
};

pub struct Catalog {
    pub ledger: DeploymentHolds,
    pub database: Database,
    pub artifacts: LocalDiskBlobStore,
    _files: tempfile::TempDir,
}

pub struct Published {
    pub id: DeploymentId,
    pub hash: String,
}

impl Catalog {
    pub async fn new(fixture: &Fixture) -> Self {
        let files = tempfile::tempdir().unwrap();
        let (url, schema) = match &fixture.admin {
            Admin::Postgres(admin) => {
                admin
                    .batch_execute(
                        "CREATE SCHEMA zeroship;
                         CREATE ROLE deployment_catalog_test LOGIN NOSUPERUSER NOCREATEDB \
                            NOCREATEROLE NOREPLICATION NOINHERIT NOBYPASSRLS;
                         GRANT CONNECT ON DATABASE postgres TO deployment_catalog_test;",
                    )
                    .await
                    .unwrap();
                admin
                    .batch_execute(deployments::POSTGRES_SCHEMA)
                    .await
                    .unwrap();
                admin
                    .batch_execute(
                        "GRANT USAGE ON SCHEMA zeroship TO deployment_catalog_test;
                         GRANT SELECT, INSERT, UPDATE, DELETE \
                            ON ALL TABLES IN SCHEMA zeroship TO deployment_catalog_test;",
                    )
                    .await
                    .unwrap();
                (
                    fixture
                        .url()
                        .replace("workflow_manager_test@", "deployment_catalog_test@"),
                    SchemaName::new("zeroship").unwrap(),
                )
            }
            Admin::Sqlite(_) => {
                let path = files.path().join("catalog.sqlite");
                rusqlite::Connection::open(&path)
                    .unwrap()
                    .execute_batch(deployments::SQLITE_SCHEMA)
                    .unwrap();
                (
                    format!("sqlite:{}", path.display()),
                    SchemaName::new("main").unwrap(),
                )
            }
        };
        let database = Database::connect(
            DbBinding::platform("deployment_catalog", "retention-test", schema),
            ConnectOptions::new(url, ProjectKeySource::unavailable()).connection_authority(),
            deployments::collections().unwrap(),
        )
        .await
        .unwrap();
        Self {
            ledger: DeploymentHolds::new(database.clone()).unwrap(),
            database,
            artifacts: LocalDiskBlobStore::new(files.path().join("artifacts")).unwrap(),
            _files: files,
        }
    }

    pub fn client(&self) -> Rc<dyn HoldClient> {
        Rc::new(CatalogClient::new(self.ledger.clone()))
    }

    pub async fn publish(
        &self,
        app: &AppId,
        marker: &str,
        schedules: &[ScheduleDescriptor],
    ) -> Published {
        let source = format!(
            "export default {{ fetch() {{ return new Response({}); }} }};",
            serde_json::to_string(marker).unwrap()
        );
        let code_hash = zeroship_bundle::sha256_hex(source.as_bytes());
        self.artifacts
            .put_blob(&code_hash, source.as_bytes())
            .await
            .unwrap();
        let manifest = Manifest {
            worker: Some(WorkerCode {
                entry: "index.js".into(),
                modules: HashMap::from([("index.js".into(), code_hash)]),
            }),
            workflows: Some(serde_json::json!(["scheduled-work"])),
            schedules: schedules
                .iter()
                .map(|schedule| {
                    let mut value = serde_json::to_value(schedule).unwrap();
                    value["input"] = serde_json::Value::Null;
                    value
                })
                .collect(),
            ..Manifest::default()
        };
        let mut value = serde_json::to_value(manifest).unwrap();
        let hash = zeroship_bundle::deployment_manifest_hash(&serde_json::to_vec(&value).unwrap())
            .unwrap();
        value["deploy_hash"] = serde_json::json!(hash);
        let encoded = serde_json::to_string(&value).unwrap();
        self.artifacts
            .put_manifest(app, &hash, encoded.as_bytes())
            .await
            .unwrap();
        let id = self
            .ledger
            .record_deployment(app, &hash, &encoded)
            .await
            .unwrap();
        Published {
            id: DeploymentId::parse(&id).unwrap(),
            hash,
        }
    }

    pub async fn assert_retained(&self, app: &AppId, deployment: &Published) {
        assert!(matches!(
            deployments::fence_reclamation(&self.database, app, deployment.id.as_str()).await,
            Err(deployments::Error::Conflict(_))
        ));
        self.artifacts
            .get_manifest(app, &deployment.hash)
            .await
            .expect("retained normal app manifest must remain readable");
    }

    pub async fn reclaim(&self, app: &AppId, deployment: &Published) {
        let hash = deployments::fence_reclamation(&self.database, app, deployment.id.as_str())
            .await
            .unwrap();
        assert_eq!(hash, deployment.hash);
        assert!(self.artifacts.delete_manifest(app, &hash).await.unwrap());
        deployments::finish_reclamation(&self.database, app, deployment.id.as_str())
            .await
            .unwrap();
        assert!(matches!(
            self.artifacts.get_manifest(app, &hash).await,
            Err(zeroship_bundle::BlobError::NotFound(_))
        ));
    }
}
