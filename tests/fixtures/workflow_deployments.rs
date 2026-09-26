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
    deploy_registrations::DeployRegistrationSource,
    deployment_holds::{DeploymentHoldAuthority, DeploymentHoldClient},
    service::{AppDeployments, BundleDeclarations, DeployRegistration, WorkflowService},
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
        assert_eq!(loaded.primary_schema(), self.descriptor.as_ref());
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
    /// A host serving exactly `apps`, each through this catalog's own client.
    pub fn binding(&self, apps: &[&AppId]) -> AppDeployments {
        self.hosting(apps, None)
    }
    /// The same host, with `client` standing in for the app it is scoped to.
    pub fn binding_with(
        &self,
        apps: &[&AppId],
        client: Rc<dyn DeploymentHoldClient>,
    ) -> AppDeployments {
        self.hosting(apps, Some(client))
    }
    fn hosting(
        &self,
        apps: &[&AppId],
        client: Option<Rc<dyn DeploymentHoldClient>>,
    ) -> AppDeployments {
        let hosted = HostedApps::new(
            apps.iter()
                .map(|app| Rc::new(self.client(app)) as Rc<dyn DeploymentHoldClient>)
                .chain(client),
        );
        AppDeployments::new(self.source.clone(), 1024 * 1024, Rc::new(hosted)).unwrap()
    }
    /// A host holding the retention authority for `apps` and nothing else: no
    /// artifact store and no asserted registration source.
    ///
    /// The control arm for the binding below. Every sweep that needs a manifest
    /// summary must refuse here, or a green on the asserted arm would only be
    /// saying the operation asks for nothing.
    pub fn holds_binding(&self, apps: &[&AppId]) -> AppDeployments {
        AppDeployments::holds_only(Rc::new(HostedApps::new(
            apps.iter()
                .map(|app| Rc::new(self.client(app)) as Rc<dyn DeploymentHoldClient>),
        )))
    }
    /// The same host, plus the asserted manifest summary. Still no artifact
    /// store: [`AppDeployments::read`] refuses on this binding by name.
    pub fn asserted_binding(&self, apps: &[&AppId]) -> AppDeployments {
        self.holds_binding(apps)
            .with_registrations(Rc::new(self.registrations()))
    }
    /// A registration source over this catalog's own `app_deploys` rows.
    ///
    /// It derives the summary the way Control's endpoint does, from the stored
    /// manifest and the hash beside it, so an engine test can exercise the
    /// asserted arm without an HTTP Control. That the DEPLOYED derivation agrees
    /// is a different claim, and `zeroship-control` asserts it against the real
    /// endpoint.
    #[must_use]
    pub fn registrations(&self) -> CatalogRegistrations {
        CatalogRegistrations {
            database: self.database.clone(),
            _directory: self.directory.clone(),
        }
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
            document["created_at"] = Value::TimestampMicros(0);
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

/// A test host serving a named set of apps. The set is stated once, where the
/// host is built, so no later call can add an app to it.
#[derive(Default)]
pub struct HostedApps(BTreeMap<AppId, Rc<dyn DeploymentHoldClient>>);
impl HostedApps {
    pub fn new(clients: impl IntoIterator<Item = Rc<dyn DeploymentHoldClient>>) -> Self {
        Self(
            clients
                .into_iter()
                .map(|client| (client.scope().app().clone(), client))
                .collect(),
        )
    }
}
impl DeploymentHoldAuthority for HostedApps {
    fn client(&self, app: &AppId) -> Result<Rc<dyn DeploymentHoldClient>, WorkflowServiceError> {
        self.0
            .get(app)
            .cloned()
            .ok_or(WorkflowServiceError::PermissionDenied)
    }
}

/// Control's half of the asserted registration, over this fixture's catalog.
///
/// It reads the stored manifest and the hash the row was accepted under,
/// re-verifies the binding between them, and parses the declarations - the same
/// three steps `DeploymentHoldApi::registration` takes. It reaches no blob
/// store: the manifest it parses is the catalog column, not an artifact.
#[derive(Clone)]
pub struct CatalogRegistrations {
    database: Database,
    _directory: Arc<tempfile::TempDir>,
}

#[async_trait::async_trait(?Send)]
impl DeployRegistrationSource for CatalogRegistrations {
    async fn registration(
        &self,
        app: &AppId,
        deployment: &zeroship_core::workflow_jobs::DeploymentId,
    ) -> Result<DeployRegistration, WorkflowServiceError> {
        let Output::Rows { rows, .. } = self
            .database
            .collection("app_deploys")
            .unwrap()
            .find(
                value!({"app_id":app.as_str(), "id":deployment.as_str()}),
                value!({}),
            )
            .await
            .unwrap()
        else {
            panic!("catalog rows");
        };
        let row = rows.first().ok_or(WorkflowServiceError::PermissionDenied)?;
        let hash = row["deploy_hash"].as_str().unwrap().to_owned();
        let manifest = zeroship_bundle::verify_deployment_manifest(
            row["manifest_json"].as_str().unwrap().as_bytes(),
            &hash,
        )
        .map_err(|_| WorkflowServiceError::Internal("catalog manifest".into()))?;
        Ok(BundleDeclarations::parse(&manifest)
            .map_err(|_| WorkflowServiceError::Internal("catalog declarations".into()))?
            .registration(deployment.as_str().to_owned(), hash))
    }
}

/// A source that answers about the right deployment under the WRONG hash.
///
/// The engine pins the hash from the hold it already took, so this is the arm
/// that proves it compares rather than records what it was handed.
pub struct RehashedRegistrations<T>(pub T, pub String);

#[async_trait::async_trait(?Send)]
impl<T: DeployRegistrationSource> DeployRegistrationSource for RehashedRegistrations<T> {
    async fn registration(
        &self,
        app: &AppId,
        deployment: &zeroship_core::workflow_jobs::DeploymentId,
    ) -> Result<DeployRegistration, WorkflowServiceError> {
        let mut registration = self.0.registration(app, deployment).await?;
        registration.hash.clone_from(&self.1);
        Ok(registration)
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
