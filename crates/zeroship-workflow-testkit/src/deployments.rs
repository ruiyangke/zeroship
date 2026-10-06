//! The workflow deployment catalog, its artifact store and its holds.
//!
//! A case's catalog is a `SQLite` platform file, the artifact store beside it,
//! the hold ledger the manager exposes over it and the ORM database the
//! catalog rows live in. None of that is a workflow-engine type, so the whole
//! arrangement is plain data: the consumers name their own registration,
//! binding and error types and convert at the edge.
//!
//! [`Declaration`] is the registration the catalog stores, in the plain shape
//! the manifest carries: ids and workflows are strings, and a schedule is a
//! `serde_json::Value` the caller decodes into its own type. Nothing here
//! reaches a workflow service.

#![expect(
    clippy::future_not_send,
    reason = "fixtures use their owning compio thread"
)]

use std::{collections::BTreeMap, sync::Arc};
use zeroship_bundle::{
    BlobStore, LocalDiskBlobStore, Manifest, RuntimeDescriptorEntry, WorkerCode,
};
use zeroship_core::{
    app_id::AppId,
    schema_name::SchemaName,
    typed_id,
    workflow_deployments::{HoldGeneration, HoldReceipt, HoldScope},
};
use zeroship_data_orm::{
    binding::DbBinding,
    encryption::ProjectKeySource,
    orm::{Database, Output},
    value, ConnectOptions, Value,
};
use zeroship_workflow_manager::deployments::{self as deployment_holds, DeploymentHolds};

/// A deployment's declarations in the plain shape the catalog stores.
#[derive(Clone, Debug)]
pub struct Declaration {
    pub id: String,
    pub hash: String,
    pub workflows: std::collections::BTreeSet<String>,
    /// The creator's inline schedule inputs, in the encoding the manifest
    /// carries. The consumer decodes each into its own schedule type.
    pub schedules: Vec<serde_json::Value>,
}

/// A catalog write that the stored row refuses.
#[derive(Debug)]
pub enum PublishError {
    /// The row already exists under a different app or hash.
    Conflict(String),
}

/// Real normal app artifacts.
#[derive(Clone, Debug)]
pub struct Sources {
    pub entry: String,
    pub modules: BTreeMap<String, String>,
    pub descriptor: Option<serde_json::Value>,
}
impl Default for Sources {
    fn default() -> Self {
        Self {
            entry: "index.js".into(),
            modules: [("index.js".into(), "export default {};".into())].into(),
            descriptor: None,
        }
    }
}
impl Sources {
    pub fn single(source: &str) -> Self {
        Self {
            modules: [("index.js".into(), source.to_owned())].into(),
            ..Self::default()
        }
    }
    /// Assert a loaded worker carries exactly these sources.
    pub fn assert_loaded(&self, loaded: &zeroship_bundle::LoadedWorker) {
        assert_eq!(loaded.entry(), self.entry);
        assert_eq!(loaded.modules(), &self.modules);
        assert_eq!(loaded.primary_schema(), self.descriptor.as_ref());
    }
}

/// The deployment catalog a case owns.
pub struct Catalog {
    directory: Arc<tempfile::TempDir>,
    pub source: Arc<dyn BlobStore>,
    pub database: Database,
    pub ledger: DeploymentHolds,
    /// The local platform file holding this catalog, for queues that hold
    /// deployments through it.
    pub platform: zeroship_workflow_manager::local::LocalPlatform,
}
impl Catalog {
    pub async fn new() -> Self {
        let directory = Arc::new(tempfile::tempdir().unwrap());
        let source = Arc::new(LocalDiskBlobStore::new(directory.path().join("artifacts")).unwrap());
        Self::with_source(directory, source).await
    }

    pub async fn with_source(
        directory: Arc<tempfile::TempDir>,
        source: Arc<dyn BlobStore>,
    ) -> Self {
        let path = directory.path().join("platform.sqlite");
        let platform = zeroship_workflow_manager::local::LocalPlatform::open(&path)
            .await
            .unwrap();
        let ledger = platform.deployments().clone();
        let database = Database::connect(
            DbBinding::platform(
                "platform",
                "fixture-catalog",
                SchemaName::new("main").unwrap(),
            ),
            ConnectOptions::new(
                format!("sqlite:{}", path.display()),
                ProjectKeySource::unavailable(),
            )
            .connection_authority(),
            deployment_holds::collections().unwrap(),
        )
        .await
        .unwrap();
        Self {
            directory,
            source,
            database,
            ledger,
            platform,
        }
    }

    /// A hold client scoped to one app, over this catalog's ledger.
    pub fn client_for_scope(&self, scope: HoldScope) -> HoldHandle {
        HoldHandle {
            ledger: self.ledger.clone(),
            scope,
            _directory: self.directory.clone(),
        }
    }

    /// Store `declaration`'s sources as a deployable artifact and record its
    /// catalog row, returning the registration with its pinned hash.
    ///
    /// # Errors
    /// When the stored row already names another app or another hash.
    pub async fn publish(
        &self,
        app: &AppId,
        declaration: &Declaration,
        sources: &Sources,
    ) -> Result<Declaration, PublishError> {
        let mut modules = std::collections::HashMap::new();
        for (path, source) in &sources.modules {
            let hash = zeroship_bundle::sha256_hex(source.as_bytes());
            self.source
                .put_blob(&hash, source.as_bytes())
                .await
                .unwrap();
            modules.insert(path.clone(), hash);
        }
        let descriptor = if let Some(value) = &sources.descriptor {
            let bytes = serde_json::to_vec(value).unwrap();
            let hash = zeroship_bundle::sha256_hex(&bytes);
            self.source.put_blob(&hash, &bytes).await.unwrap();
            vec![RuntimeDescriptorEntry {
                label: "main".into(),
                database_id: zeroship_core::DatabaseId::mint(),
                primary: true,
                hash,
            }]
        } else {
            Vec::new()
        };
        let manifest = Manifest {
            worker: Some(WorkerCode {
                entry: sources.entry.clone(),
                modules,
            }),
            runtime_descriptor: descriptor,
            workflows: Some(serde_json::to_value(&declaration.workflows).unwrap()),
            schedules: declaration.schedules.clone(),
            ..Manifest::default()
        };
        let mut raw = serde_json::to_value(manifest).unwrap();
        raw["fixture_deployment"] = serde_json::json!(declaration.id);
        let hash =
            zeroship_bundle::deployment_manifest_hash(&serde_json::to_vec(&raw).unwrap()).unwrap();
        raw["deploy_hash"] = serde_json::json!(hash);
        let encoded = serde_json::to_string(&raw).unwrap();
        let records = self.database.collection("app_deploys").unwrap();
        let Output::Rows(rows) = records
            .find(value!({"id":declaration.id}), value!({}))
            .await
            .unwrap()
        else {
            panic!("catalog rows");
        };
        if let Some(record) = rows.first() {
            if record["app_id"] != value!(app.as_str()) || record["deploy_hash"] != value!(hash) {
                return Err(PublishError::Conflict(
                    "fixture deployment is immutable".into(),
                ));
            }
        } else {
            let mut document = value!({"id":declaration.id, "app_id":app.as_str(), "deploy_hash":hash,
                "manifest_json":encoded, "activated_at":null, "retention_state":"available", "retention_lock":0});
            document["created_at"] = Value::TimestampMicros(0);
            records.insert(document).await.unwrap();
        }
        self.source
            .put_manifest(app, &hash, encoded.as_bytes())
            .await
            .unwrap();
        let mut registration = Declaration {
            hash,
            ..declaration.clone()
        };
        registration
            .schedules
            .sort_by_key(schedule_name);
        Ok(registration)
    }

    /// Publish a default deployment for `app` and return its registration.
    pub async fn deploy(&self, app: &AppId) -> Declaration {
        self.publish(
            app,
            &Declaration {
                id: typed_id::generate("dep"),
                hash: String::new(),
                workflows: ["Example".into()].into(),
                schedules: vec![],
            },
            &Sources::default(),
        )
        .await
        .unwrap()
    }

    /// The stored manifest and its hash for `app`'s `deployment`, if the row
    /// exists. The caller verifies the binding and parses the declarations.
    pub async fn registration_row(
        &self,
        app: &AppId,
        deployment: &str,
    ) -> Option<(String, Vec<u8>)> {
        self.registration_source()
            .registration_row(app, deployment)
            .await
    }

    /// A cloneable handle over the catalog rows, for a consumer that reads
    /// registrations after the catalog's own borrow has ended.
    #[must_use]
    pub fn registration_source(&self) -> RegistrationSource {
        RegistrationSource {
            database: self.database.clone(),
            _directory: self.directory.clone(),
        }
    }

    pub async fn assert_held(&self, app: &AppId, deployment: &str) {
        let rolled_back = self
            .database
            .transaction(async |tx| {
                assert!(matches!(
                    deployment_holds::fence_reclamation(&tx, app, deployment).await,
                    Err(deployment_holds::Error::Conflict(_))
                ));
                Err::<(), _>(zeroship_data_orm::error::DbError::validation(
                    "fixture_rollback",
                    "rollback deployment probe",
                ))
            })
            .await;
        assert!(matches!(
            rolled_back,
            Err(zeroship_data_orm::error::DbError::ValidationFailed {
                code: "fixture_rollback",
                ..
            })
        ));
    }

    /// Every holder gave this deployment back, so the platform collector's
    /// fence commits. The probe rolls back, leaving the catalog collectable.
    pub async fn assert_reclaimable(&self, app: &AppId, deployment: &str) {
        let rolled_back = self
            .database
            .transaction(async |tx| {
                deployment_holds::fence_reclamation(&tx, app, deployment)
                    .await
                    .expect("released holders leave a collectable deployment");
                Err::<(), _>(zeroship_data_orm::error::DbError::validation(
                    "fixture_rollback",
                    "rollback deployment probe",
                ))
            })
            .await;
        assert!(matches!(
            rolled_back,
            Err(zeroship_data_orm::error::DbError::ValidationFailed {
                code: "fixture_rollback",
                ..
            })
        ));
    }
}

/// A cloneable handle over one catalog's rows.
#[derive(Clone)]
pub struct RegistrationSource {
    database: Database,
    _directory: Arc<tempfile::TempDir>,
}
impl RegistrationSource {
    /// The stored manifest and its hash for `app`'s `deployment`, if the row
    /// exists. The caller verifies the binding and parses the declarations.
    pub async fn registration_row(
        &self,
        app: &AppId,
        deployment: &str,
    ) -> Option<(String, Vec<u8>)> {
        let Output::Rows(rows) = self
            .database
            .collection("app_deploys")
            .unwrap()
            .find(
                value!({"app_id":app.as_str(), "id":deployment}),
                value!({}),
            )
            .await
            .unwrap()
        else {
            panic!("catalog rows");
        };
        rows.first().map(|row| {
            (
                row["deploy_hash"].as_str().unwrap().to_owned(),
                row["manifest_json"].as_str().unwrap().as_bytes().to_vec(),
            )
        })
    }
}

/// The name a schedule JSON value is sorted by.
fn schedule_name(value: &serde_json::Value) -> String {
    value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// A hold the ledger refuses, in the plain shape the workflow service maps to
/// its own error.
#[derive(Debug)]
pub enum HoldError {
    InvalidRequest(String),
    Unauthenticated,
    PermissionDenied,
    Conflict(String),
    ResourceExhausted(String),
    Unavailable(String),
    Timeout,
    Internal(String),
}

/// A case's holds on one app's deployments, over the catalog's ledger.
#[derive(Clone)]
pub struct HoldHandle {
    ledger: DeploymentHolds,
    scope: HoldScope,
    _directory: Arc<tempfile::TempDir>,
}
impl HoldHandle {
    #[must_use]
    pub fn scope(&self) -> &HoldScope {
        &self.scope
    }

    /// # Errors
    /// When the ledger refuses the hold.
    pub async fn acquire(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, HoldError> {
        self.ledger
            .acquire(&self.scope, deployment, generation)
            .await
            .map_err(hold_error)
    }

    /// # Errors
    /// When the ledger refuses the release.
    pub async fn release(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, HoldError> {
        self.ledger
            .release(&self.scope, deployment, generation)
            .await
            .map_err(hold_error)
    }
}

/// The plain error a manager hold failure maps to.
fn hold_error(error: deployment_holds::Error) -> HoldError {
    use deployment_holds::Error;
    match error {
        Error::InvalidRequest(message) => HoldError::InvalidRequest(message),
        Error::Unauthenticated => HoldError::Unauthenticated,
        Error::PermissionDenied => HoldError::PermissionDenied,
        Error::Conflict(message) => HoldError::Conflict(message),
        Error::ResourceExhausted(message) => HoldError::ResourceExhausted(message),
        Error::Unavailable(message) => HoldError::Unavailable(message),
        Error::Timeout => HoldError::Timeout,
        Error::Internal(message) => HoldError::Internal(message),
    }
}

impl std::fmt::Debug for Catalog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Catalog").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for RegistrationSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("RegistrationSource").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for HoldHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("HoldHandle").finish_non_exhaustive()
    }
}
