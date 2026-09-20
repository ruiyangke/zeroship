use crate::support::{Admin, Fixture};
use zeroship_core::{app_id::AppId, schema_name::SchemaName};
use zeroship_data_orm::{
    binding::DbBinding,
    encryption::ProjectKeySource,
    orm::{Database, Operation, Output},
    schema::Schema,
    value, ConnectOptions, Value,
};
use zeroship_workflow_manager::deployments::{
    self,
    latest::{self, LatestDeployment, LatestDeploymentSource},
    DeploymentHolds,
};

pub struct Source {
    pub database: Database,
    pub latest: LatestDeploymentSource,
    pub ledger: DeploymentHolds,
}

impl Source {
    pub async fn new(fixture: &Fixture) -> Self {
        let (namespace, writer_url) = match &fixture.admin {
            Admin::Postgres(admin) => {
                admin
                    .batch_execute(
                        "CREATE SCHEMA zeroship;
                    CREATE TABLE zeroship.apps (id text PRIMARY KEY, deploy_hash text);
                    GRANT USAGE ON SCHEMA zeroship TO workflow_manager_test;
                    GRANT SELECT (id, deploy_hash) ON zeroship.apps TO workflow_manager_test;",
                    )
                    .await
                    .unwrap();
                admin
                    .batch_execute(deployments::POSTGRES_SCHEMA)
                    .await
                    .unwrap();
                admin
                    .batch_execute(
                        "GRANT SELECT (id, app_id, deploy_hash, retention_state)
                    ON zeroship.app_deploys TO workflow_manager_test;",
                    )
                    .await
                    .unwrap();
                (
                    "zeroship",
                    fixture.url().replace("workflow_manager_test@", "postgres@"),
                )
            }
            Admin::Sqlite(admin) => {
                admin
                    .execute_batch(
                        "CREATE TABLE apps (id TEXT PRIMARY KEY NOT NULL, deploy_hash TEXT);",
                    )
                    .unwrap();
                admin.execute_batch(deployments::SQLITE_SCHEMA).unwrap();
                ("main", fixture.url().to_owned())
            }
        };
        let binding = DbBinding::platform(
            "platform",
            "management-selection",
            SchemaName::new(namespace).unwrap(),
        );
        let mut schema = latest::collections().unwrap().into_collections();
        schema.extend(
            deployments::collections()
                .unwrap()
                .into_collections()
                .into_iter()
                .filter(|(name, _)| name == "app_deploy_holds"),
        );
        let database = Database::connect(
            binding.clone(),
            ConnectOptions::new(writer_url, ProjectKeySource::unavailable()).connection_authority(),
            Schema::new(schema),
        )
        .await
        .unwrap();
        let reader = Database::connect(
            binding,
            ConnectOptions::new(fixture.url(), ProjectKeySource::unavailable())
                .connection_authority(),
            latest::collections().unwrap(),
        )
        .await
        .unwrap();
        Self {
            latest: LatestDeploymentSource::new(reader).unwrap(),
            ledger: DeploymentHolds::new(database.clone()).unwrap(),
            database,
        }
    }

    pub async fn publish(&self, app: &AppId, marker: &str) -> LatestDeployment {
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
        LatestDeployment {
            app_id: app.clone(),
            deployment_id: zeroship_core::workflow_jobs::DeploymentId::parse(&id).unwrap(),
            deploy_hash: hash,
        }
    }

    pub async fn select(&self, app: &AppId, hash: Option<&str>) {
        let apps = self.database.collection("apps").unwrap();
        let Output::Rows { rows, .. } = apps
            .find(value!({"id":app.as_str()}), value!({"limit":1}))
            .await
            .unwrap()
        else {
            panic!("expected app query");
        };
        if rows.is_empty() {
            apps.insert(value!({"id":app.as_str(),"deploy_hash":hash}))
                .await
                .unwrap();
        } else {
            self.patch("apps", app.as_str(), value!({"deploy_hash":hash}))
                .await;
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
