#![recursion_limit = "256"]
#![expect(
    clippy::future_not_send,
    reason = "native source fixtures own compio-local database handles"
)]

#[allow(dead_code, reason = "shared fixture also supports manager queue tests")]
mod support;

use std::collections::BTreeMap;
use support::{Admin, Backend, Fixture};
use zeroship_core::{app_id::AppId, schema_name::SchemaName, workflow_jobs::DeploymentId};
use zeroship_data_orm::{
    binding::DbBinding,
    encryption::ProjectKeySource,
    orm::{Database, FromRow, Operation, Output, UtcInstant},
    schema::Schema,
    value, ConnectOptions, Value,
};
use zeroship_workflow_manager::{
    deployments::{
        self,
        latest::{self, LatestDeployment, LatestDeploymentSource},
        models::app_deploys as deploys,
        DeploymentHolds,
    },
    Error,
};

macro_rules! case {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[compio::test]
        async fn $sqlite() {
            Box::pin($contract(LatestFixture::new(Backend::Sqlite).await)).await;
        }
        #[compio::test]
        async fn $postgres() {
            Box::pin($contract(LatestFixture::new(Backend::Postgres).await)).await;
        }
    };
}

case!(
    sqlite_latest_uses_current_pointer_without_writes,
    postgres_latest_uses_current_pointer_without_writes,
    current_pointer
);
case!(
    sqlite_latest_refuses_missing_foreign_and_unavailable_targets,
    postgres_latest_refuses_missing_foreign_and_unavailable_targets,
    unavailable_target
);
case!(
    sqlite_latest_rejects_malformed_storage,
    postgres_latest_rejects_malformed_storage,
    malformed_storage
);
case!(
    sqlite_activation_instants_survive_the_catalog,
    postgres_activation_instants_survive_the_catalog,
    activation_instant_round_trip
);

struct LatestFixture {
    infrastructure: Fixture,
    writer: Database,
    reader: Database,
    source: LatestDeploymentSource,
}

impl LatestFixture {
    async fn new(backend: Backend) -> Self {
        let infrastructure = Fixture::new(backend).await;
        let (schema, writer_url) = match &infrastructure.admin {
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
                    infrastructure
                        .url()
                        .replace("workflow_manager_test@", "postgres@"),
                )
            }
            Admin::Sqlite(admin) => {
                admin
                    .execute_batch(
                        "CREATE TABLE apps (id TEXT PRIMARY KEY NOT NULL, deploy_hash TEXT);",
                    )
                    .unwrap();
                admin.execute_batch(deployments::SQLITE_SCHEMA).unwrap();
                ("main", infrastructure.url().to_owned())
            }
        };
        let binding = DbBinding::new(
            "platform",
            "latest-deployment-test",
            SchemaName::new(schema).unwrap(),
        );
        let mut metadata = latest::collections().unwrap().into_collections();
        metadata.extend(
            deployments::collections()
                .unwrap()
                .into_collections()
                .into_iter()
                .filter(|(name, _)| name == "app_deploy_holds"),
        );
        let writer = Database::connect(
            binding.clone(),
            ConnectOptions::new(writer_url, ProjectKeySource::unavailable()).connection_authority(),
            Schema::new(metadata),
        )
        .await
        .unwrap();
        let reader = Database::connect(
            binding,
            ConnectOptions::new(infrastructure.url(), ProjectKeySource::unavailable())
                .connection_authority(),
            latest::collections().unwrap(),
        )
        .await
        .unwrap();
        let source = LatestDeploymentSource::new(reader.clone()).unwrap();
        Self {
            infrastructure,
            writer,
            reader,
            source,
        }
    }

    async fn insert_app(&self, app: &AppId, hash: Option<&str>) {
        self.writer
            .collection("apps")
            .unwrap()
            .insert(value!({"id":app.as_str(),"deploy_hash":hash}))
            .await
            .unwrap();
    }

    async fn patch(&self, collection: &str, id: &str, changes: Value) {
        let output = self
            .writer
            .collection(collection)
            .unwrap()
            .execute(Operation::Update {
                filter: value!({"id":id}),
                patch: changes,
                many: true,
            })
            .await
            .unwrap();
        assert!(matches!(output, Output::Count(1)));
    }

    async fn publish(&self, app: &AppId, marker: &str) -> LatestDeployment {
        let mut manifest = serde_json::to_value(zeroship_bundle::Manifest::default()).unwrap();
        manifest["fixture_marker"] = serde_json::json!(marker);
        let hash =
            zeroship_bundle::deployment_manifest_hash(&serde_json::to_vec(&manifest).unwrap())
                .unwrap();
        manifest["deploy_hash"] = serde_json::json!(hash);
        let id = DeploymentHolds::new(self.writer.clone())
            .unwrap()
            .record_deployment(app, &hash, &serde_json::to_string(&manifest).unwrap())
            .await
            .unwrap();
        LatestDeployment {
            app_id: app.clone(),
            deployment_id: DeploymentId::parse(&id).unwrap(),
            deploy_hash: hash,
        }
    }

    async fn write_guard(&self, enabled: bool) {
        let Admin::Sqlite(_) = &self.infrastructure.admin else {
            return;
        };
        let mut statements = String::new();
        for table in ["apps", "app_deploys", "app_deploy_holds"] {
            for operation in ["INSERT", "UPDATE", "DELETE"] {
                let trigger = format!("latest_read_only_{table}_{operation}");
                let ddl = if enabled {
                    format!(
                        "CREATE TRIGGER {trigger} BEFORE {operation} ON {table}
                             BEGIN SELECT RAISE(ABORT, 'latest source must not write'); END;"
                    )
                } else {
                    format!("DROP TRIGGER {trigger};")
                };
                statements.push_str(&ddl);
            }
        }
        let path = self.infrastructure.url().to_owned();
        compio::runtime::spawn_blocking(move || {
            rusqlite::Connection::open(path)?.execute_batch(&statements)
        })
        .await
        .unwrap()
        .unwrap();
    }

    async fn snapshot(&self) -> BTreeMap<&'static str, Vec<Value>> {
        let mut result = BTreeMap::new();
        for table in ["apps", "app_deploys", "app_deploy_holds"] {
            let Output::Rows { rows, .. } = self
                .writer
                .collection(table)
                .unwrap()
                .find(value!({}), value!({"orderBy":{"id":1},"limit":256}))
                .await
                .unwrap()
            else {
                panic!("expected deployment source rows")
            };
            result.insert(table, rows);
        }
        result
    }

    async fn observe(&self, app: &AppId) -> Result<LatestDeployment, Error> {
        let before = self.snapshot().await;
        self.write_guard(true).await;
        let observed = self.source.observe(app).await;
        let after = self.snapshot().await;
        self.write_guard(false).await;
        assert_eq!(
            after, before,
            "selection must leave the catalog and holds unchanged"
        );
        observed
    }

    async fn assert_write_rejected(&self, app: &AppId) {
        self.write_guard(true).await;
        let result = self
            .reader
            .collection("apps")
            .unwrap()
            .update(value!({"id":app.as_str()}), value!({"deploy_hash":null}))
            .await;
        self.write_guard(false).await;
        assert!(result.is_err(), "the fixture must reject source writes");
    }
}

async fn current_pointer(fixture: LatestFixture) {
    let app = AppId::mint();
    let current = fixture.publish(&app, "current").await;
    let later = fixture.publish(&app, "later timestamp").await;
    assert_ne!(current.deployment_id, later.deployment_id);
    assert_ne!(current.deploy_hash, later.deploy_hash);
    fixture.insert_app(&app, Some(&current.deploy_hash)).await;
    fixture
        .patch(
            "app_deploys",
            current.deployment_id.as_str(),
            value!({"activated_at":null}),
        )
        .await;
    fixture
        .patch(
            "app_deploys",
            later.deployment_id.as_str(),
            value!({"activated_at":Value::TimestampMicros(1000)}),
        )
        .await;
    fixture.assert_write_rejected(&app).await;
    let original = fixture.observe(&app).await.unwrap();
    assert_eq!(original, current);
    fixture
        .patch(
            "apps",
            app.as_str(),
            value!({"deploy_hash":later.deploy_hash}),
        )
        .await;
    assert_eq!(fixture.observe(&app).await.unwrap(), later);
    assert_eq!(original, current);
    fixture
        .patch(
            "apps",
            app.as_str(),
            value!({"deploy_hash":current.deploy_hash}),
        )
        .await;
    assert_eq!(fixture.observe(&app).await.unwrap(), current);
    assert!(fixture.snapshot().await["app_deploy_holds"].is_empty());
}

async fn unavailable_target(fixture: LatestFixture) {
    let owner = AppId::mint();
    let foreign = AppId::mint();
    assert_eq!(fixture.observe(&owner).await, Err(Error::Unavailable));
    fixture.insert_app(&owner, None).await;
    let fallback = fixture.publish(&owner, "old available").await;
    assert_eq!(fixture.observe(&owner).await, Err(Error::Unavailable));
    let foreign_only = fixture.publish(&foreign, "foreign only").await;
    fixture
        .insert_app(&foreign, Some(&foreign_only.deploy_hash))
        .await;
    fixture
        .patch(
            "apps",
            owner.as_str(),
            value!({"deploy_hash":foreign_only.deploy_hash}),
        )
        .await;
    assert_eq!(fixture.observe(&owner).await, Err(Error::Unavailable));
    assert_eq!(fixture.observe(&foreign).await.unwrap(), foreign_only);
    let owner_copy = fixture.publish(&owner, "foreign only").await;
    assert_eq!(owner_copy.deploy_hash, foreign_only.deploy_hash);
    assert_ne!(owner_copy.deployment_id, foreign_only.deployment_id);
    assert_eq!(fixture.observe(&owner).await.unwrap(), owner_copy);
    for state in ["reclaiming", "deleted"] {
        fixture
            .patch(
                "app_deploys",
                owner_copy.deployment_id.as_str(),
                value!({"retention_state":state}),
            )
            .await;
        assert_eq!(fixture.observe(&owner).await, Err(Error::Unavailable));
        assert_eq!(fixture.observe(&foreign).await.unwrap(), foreign_only);
    }
    fixture
        .patch(
            "apps",
            owner.as_str(),
            value!({"deploy_hash":zeroship_bundle::sha256_hex(b"missing deployment")}),
        )
        .await;
    assert_eq!(fixture.observe(&owner).await, Err(Error::Unavailable));
    fixture
        .patch(
            "apps",
            owner.as_str(),
            value!({"deploy_hash":fallback.deploy_hash}),
        )
        .await;
    assert_eq!(fixture.observe(&owner).await.unwrap(), fallback);
}

async fn malformed_storage(fixture: LatestFixture) {
    let app = AppId::mint();
    let deployment = fixture.publish(&app, "valid identity").await;
    fixture
        .insert_app(&app, Some(&deployment.deploy_hash))
        .await;
    for hash in ["", "not-a-deployment-hash"] {
        fixture
            .patch("apps", app.as_str(), value!({"deploy_hash":hash}))
            .await;
        assert_eq!(fixture.observe(&app).await, Err(Error::Storage));
    }
    fixture
        .patch(
            "apps",
            app.as_str(),
            value!({"deploy_hash":deployment.deploy_hash}),
        )
        .await;
    fixture
        .patch(
            "app_deploys",
            deployment.deployment_id.as_str(),
            value!({"retention_state":"unknown"}),
        )
        .await;
    assert_eq!(fixture.observe(&app).await, Err(Error::Storage));
    fixture
        .patch(
            "app_deploys",
            deployment.deployment_id.as_str(),
            value!({"retention_state":"available","deploy_hash":"broken"}),
        )
        .await;
    fixture
        .patch("apps", app.as_str(), value!({"deploy_hash":"broken"}))
        .await;
    assert_eq!(fixture.observe(&app).await, Err(Error::Storage));
    fixture
        .patch(
            "app_deploys",
            deployment.deployment_id.as_str(),
            value!({"deploy_hash":deployment.deploy_hash}),
        )
        .await;
    fixture
        .patch(
            "apps",
            app.as_str(),
            value!({"deploy_hash":deployment.deploy_hash}),
        )
        .await;
    assert_eq!(fixture.observe(&app).await.unwrap(), deployment);

    let bad_hash = zeroship_bundle::sha256_hex(b"malformed stored deployment identity");
    fixture
        .writer
        .collection("app_deploys")
        .unwrap()
        .insert(value!({
            "id":"malformed-deployment-id", "app_id":app.as_str(), "deploy_hash":bad_hash,
            "manifest_json":"unread", "created_at":Value::TimestampMicros(0), "activated_at":null,
            "retention_state":"available", "retention_lock":0,
        }))
        .await
        .unwrap();
    fixture
        .patch("apps", app.as_str(), value!({"deploy_hash":bad_hash}))
        .await;
    assert_eq!(fixture.observe(&app).await, Err(Error::Storage));
    fixture
        .patch(
            "apps",
            app.as_str(),
            value!({"deploy_hash":deployment.deploy_hash}),
        )
        .await;
    assert_eq!(fixture.observe(&app).await.unwrap(), deployment);
}

/// `app_deploys.activated_at` as the retention fence reads it: the column is a
/// timestamp, so the field is a [`UtcInstant`] and a bare integer would not
/// compile against it.
#[derive(FromRow)]
#[orm(entity = deploys)]
struct Activation {
    activated_at: Option<UtcInstant>,
}

/// An activation instant written through the catalog comes back as the same
/// microseconds, and an unactivated deployment comes back with none.
///
/// The value is what is asserted, not merely that the read succeeded: a codec
/// that scaled milliseconds against microseconds would return an instant three
/// orders of magnitude away and still report success. The instant is a whole
/// millisecond because `SQLite`'s canonical timestamp text keeps milliseconds
/// and refuses a finer value rather than flooring it.
///
/// This binds the column's codec and unit only. Whether the collector compares
/// the value it reads against the right cutoff is a separate contract.
async fn activation_instant_round_trip(fixture: LatestFixture) {
    const ACTIVATED_MICROS: i64 = 1_789_279_200_004_000;
    let app = AppId::mint();
    let activated = fixture.publish(&app, "activated").await;
    let never = fixture.publish(&app, "never activated").await;
    fixture
        .patch(
            "app_deploys",
            activated.deployment_id.as_str(),
            value!({"activated_at":Value::TimestampMicros(ACTIVATED_MICROS)}),
        )
        .await;
    let read = async |id: &DeploymentId| {
        fixture
            .writer
            .entity::<deploys::Entity>()
            .unwrap()
            .query()
            .filter(deploys::id.eq(id.as_str()).unwrap())
            .first::<Activation>()
            .await
            .unwrap()
            .expect("the published deployment is in the catalog")
            .activated_at
    };
    assert_eq!(
        read(&activated.deployment_id)
            .await
            .map(UtcInstant::unix_micros),
        Some(ACTIVATED_MICROS),
    );
    // Control, differing only in whether the column was ever set.
    assert_eq!(read(&never.deployment_id).await, None);
}
