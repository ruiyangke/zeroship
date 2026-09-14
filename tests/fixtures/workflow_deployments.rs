//! Real normal app artifacts and an independent ORM deployment catalog.

#![allow(
    dead_code,
    reason = "fixture consumers exercise different deployment contracts"
)]
#![expect(
    clippy::future_not_send,
    reason = "fixtures use their owning compio thread"
)]

use std::{collections::BTreeMap, rc::Rc, sync::Arc};
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
use zeroship_workflow::{
    deployment_holds::DeploymentHoldClient,
    service::{AppDeployments, DeployRegistration, WorkflowService},
    WorkflowServiceError,
};
use zeroship_workflow_manager::deployments::{self as deployment_holds, DeploymentHolds};

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
    pub fn assert_loaded(&self, loaded: &zeroship_bundle::LoadedWorker) {
        assert_eq!(loaded.entry(), self.entry);
        assert_eq!(loaded.modules(), &self.modules);
        assert_eq!(loaded.runtime_descriptor(), self.descriptor.as_ref());
    }
}

pub struct Deployments {
    directory: Arc<tempfile::TempDir>,
    pub source: Arc<dyn BlobStore>,
    pub database: Database,
    pub ledger: DeploymentHolds,
    /// The local platform file holding this catalog, for queues that hold
    /// deployments through it.
    pub platform: zeroship_workflow_manager::local::LocalPlatform,
}
impl Deployments {
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
            DbBinding::new(
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
    pub fn client(&self, app: &AppId) -> OwnedClient {
        self.client_for_scope(HoldScope::for_app(app.clone()))
    }
    pub fn client_for_scope(&self, scope: HoldScope) -> OwnedClient {
        OwnedClient {
            ledger: self.ledger.clone(),
            scope,
            _directory: self.directory.clone(),
        }
    }
    pub fn binding(&self, apps: &[&AppId]) -> AppDeployments {
        apps.iter().fold(
            AppDeployments::new(self.source.clone(), 1024 * 1024).unwrap(),
            |binding, app| binding.with_hold_client(Rc::new(self.client(app))),
        )
    }
    pub async fn publish(
        &self,
        app: &AppId,
        declaration: &DeployRegistration,
        sources: &Sources,
    ) -> Result<DeployRegistration, WorkflowServiceError> {
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
            Some(RuntimeDescriptorEntry { hash })
        } else {
            None
        };
        let manifest = Manifest {
            worker: Some(WorkerCode {
                entry: sources.entry.clone(),
                modules,
            }),
            runtime_descriptor: descriptor,
            workflows: Some(serde_json::to_value(&declaration.workflows).unwrap()),
            schedules: declaration
                .schedules
                .iter()
                .map(|s| serde_json::to_value(s).unwrap())
                .collect(),
            ..Manifest::default()
        };
        let mut raw = serde_json::to_value(manifest).unwrap();
        raw["fixture_deployment"] = serde_json::json!(declaration.id);
        let hash =
            zeroship_bundle::deployment_manifest_hash(&serde_json::to_vec(&raw).unwrap()).unwrap();
        raw["deploy_hash"] = serde_json::json!(hash);
        let encoded = serde_json::to_string(&raw).unwrap();
        let records = self.database.collection("app_deploys").unwrap();
        let Output::Rows { rows, .. } = records
            .find(value!({"id":declaration.id}), value!({}))
            .await
            .unwrap()
        else {
            panic!("catalog rows");
        };
        if let Some(record) = rows.first() {
            if record["app_id"] != value!(app.as_str()) || record["deploy_hash"] != value!(hash) {
                return Err(WorkflowServiceError::Conflict(
                    "fixture deployment is immutable".into(),
                ));
            }
        } else {
            let mut document = value!({"id":declaration.id, "app_id":app.as_str(), "deploy_hash":hash,
                "manifest_json":encoded, "activated_at":null, "retention_state":"available", "retention_lock":0});
            document["created_at"] = Value::Timestamp(0);
            records.insert(document).await.unwrap();
        }
        self.source
            .put_manifest(app, &hash, encoded.as_bytes())
            .await
            .unwrap();
        let mut registration = DeployRegistration {
            hash,
            ..declaration.clone()
        };
        registration
            .schedules
            .sort_by(|left, right| left.name.cmp(&right.name));
        Ok(registration)
    }
    pub async fn activate(
        &self,
        service: &WorkflowService,
        app: &AppId,
        declaration: &DeployRegistration,
    ) -> Result<(), WorkflowServiceError> {
        let deployment = self.publish(app, declaration, &Sources::default()).await?;
        service.activate_deploy(app, &deployment).await
    }
    pub async fn deploy(&self, app: &AppId) -> DeployRegistration {
        self.publish(
            app,
            &DeployRegistration {
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
}

#[derive(Clone)]
pub struct OwnedClient {
    ledger: DeploymentHolds,
    scope: HoldScope,
    _directory: Arc<tempfile::TempDir>,
}
#[async_trait::async_trait(?Send)]
impl DeploymentHoldClient for OwnedClient {
    fn scope(&self) -> &HoldScope {
        &self.scope
    }
    async fn acquire(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.ledger
            .acquire(&self.scope, deployment, generation)
            .await
            .map_err(deployment_error)
    }
    async fn release(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.ledger
            .release(&self.scope, deployment, generation)
            .await
            .map_err(deployment_error)
    }
}

fn deployment_error(error: zeroship_workflow_manager::deployments::Error) -> WorkflowServiceError {
    use zeroship_workflow_manager::deployments::Error;
    match error {
        Error::InvalidRequest(message) => WorkflowServiceError::InvalidRequest(message),
        Error::Unauthenticated => WorkflowServiceError::Unauthenticated,
        Error::PermissionDenied => WorkflowServiceError::PermissionDenied,
        Error::Conflict(message) => WorkflowServiceError::Conflict(message),
        Error::ResourceExhausted(message) => WorkflowServiceError::ResourceExhausted(message),
        Error::Unavailable(message) => WorkflowServiceError::Unavailable(message),
        Error::Timeout => WorkflowServiceError::Timeout,
        Error::Internal(message) => WorkflowServiceError::Internal(message),
    }
}
