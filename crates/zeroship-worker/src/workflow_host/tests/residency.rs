//! A prepared app holds its credentials: what Control supplies for it stays
//! supplied while the prepared app or an execution from it exists, and leaves
//! with the last holder.

use super::*;
use crate::control_fixture::{route, ControlPlane};
use crate::residency::AppResidency;
use futures::future::Either;
use zeroship_bundle::LocalDiskBlobStore;
use zeroship_core::{
    database_role::DatabaseCapability,
    project_data_key::ProjectDataKey,
    service_assertion::{ServiceIssuer, ServiceTrustBundle, TransportAssertionVerifier},
    service_identity::endpoints,
    service_peers::{service_issuer, InstanceSigningKey, WORKER_SERVICE_NAME},
    types::{AppVersionInfo, VersionMap},
    workflow_coordination::WorkerId,
    BindingId, DatabaseId, ProjectId,
};
use zeroship_data_orm::encryption::{
    canonical_aad, decrypt, derive_key, encrypt, AeadKey, KeyStore, ProjectKeySource,
};
use zeroship_workflow_runner::prepared::{PreparedApps, PreparedOptions};

/// The worker's process stores, and the registry the host holds apps in.
struct Host {
    _storage: tempfile::TempDir,
    auth: Arc<ServiceAuth>,
    db: Arc<DbService>,
    envs: SharedEnvs,
    versions: SharedVersions,
    residency: AppResidency,
    storage: StorageBackendConfig,
    blobs: Arc<dyn BlobStore>,
}

impl Host {
    fn new() -> Self {
        let storage = tempfile::tempdir().expect("private worker storage");
        let db = crate::cache::fixture::database_service(
            "postgresql://fixture:fixture@localhost/unused",
        );
        let envs = SharedEnvs::default();
        let residency = AppResidency::new(
            Some(db.project_keys().clone()),
            Some(db.app_bindings().clone()),
            envs.clone(),
        );
        Self {
            auth: instance_auth(),
            db,
            envs,
            versions: SharedVersions::default(),
            residency,
            storage: StorageBackendConfig::Local(storage.path().join("objects")),
            blobs: Arc::new(
                LocalDiskBlobStore::new(storage.path().join("blobs")).expect("blob storage"),
            ),
            _storage: storage,
        }
    }

    /// The host's production resource provider, reading Control at `control_url`.
    fn provider(&self, control_url: &str) -> ProductionResources {
        ProductionResources::open(HostResources {
            service_auth: self.auth.clone(),
            control_url: control_url.to_owned(),
            db_service: self.db.clone(),
            storage: self.storage.clone(),
            kv_store: None,
            blob_store: self.blobs.clone(),
            meter: Arc::new(zeroship_metering::Meter::new()),
            versions: self.versions.clone(),
            envs: self.envs.clone(),
            residency: self.residency.clone(),
        })
        .expect("the provider opens over local storage")
    }

    /// Which of the app's credentials this worker holds: key, bindings, env.
    fn supplied(&self, app: &AppId) -> [bool; 3] {
        [
            self.db
                .project_keys()
                .is_bound(app.as_str())
                .expect("key store"),
            self.db
                .app_bindings()
                .is_bound(app.as_str())
                .expect("binding store"),
            sync::get_env(&self.envs, app).is_some(),
        ]
    }

    /// Read one cell sealed under `root` the way a session does: the key is
    /// looked up in the host's store on this call.
    async fn read(&self, app: &AppId, root: [u8; 32]) -> Result<Vec<u8>, String> {
        let database = DatabaseId::mint();
        let aad = canonical_aad(&database, "notes", "body", b"row-1");
        let blob = encrypt(
            &derive_key(&AeadKey { k_enc: root }, &database),
            b"held",
            &aad,
        )
        .expect("the cell seals");
        let key = KeyStore::new(ProjectKeySource::supplied(self.db.project_keys().clone()))
            .resolve(app.as_str(), &database)
            .await
            .map_err(|error| error.to_string())?;
        decrypt(&key, &blob, &aad).map_err(|error| error.to_string())
    }
}

/// An enrolled instance identity; Control's fixture does not verify it, and the
/// workflow client takes the worker id from it.
fn instance_auth() -> Arc<ServiceAuth> {
    let role = service_issuer(WORKER_SERVICE_NAME).expect("worker issuer");
    let instance =
        ServiceIssuer::parse(&format!("{}/{}", role.as_str(), WorkerId::mint().as_str()))
            .expect("worker instance issuer");
    let keyring = InstanceSigningKey::generate()
        .into_keyring(instance, ServiceTrustBundle::new())
        .expect("worker instance keyring");
    Arc::new(ServiceAuth::new(
        keyring,
        Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
    ))
}

fn version(env_version: i64) -> AppVersionInfo {
    AppVersionInfo {
        deploy_hash: None,
        plan_id: "starter".into(),
        runtime: crate::cache::TEST_LIMITS,
        env_version,
        manifest: None,
        net_policy: zeroship_core::types::AppNetPolicy::default(),
        live_bindings: std::collections::BTreeMap::new(),
    }
}

const ENV: &str = r#"{"vars":{"API_TOKEN":"held"},"secrets":{},"expose":[]}"#;

/// Everything Control serves a worker preparing `app` from nothing.
fn served(app: &AppId, root: [u8; 32]) -> Vec<(String, String)> {
    vec![
        (
            route(endpoints::CONTROL_APP, app),
            serde_json::to_string(&version(1)).expect("version entry"),
        ),
        (
            route(endpoints::CONTROL_APP_DATA_KEY, app),
            serde_json::to_string(&ProjectDataKey::new(ProjectId::mint(), root))
                .expect("project key"),
        ),
        (
            route(endpoints::CONTROL_APP_BINDINGS, app),
            serde_json::json!({"bindings": [{
                "binding_id": BindingId::mint().as_str(),
                "database_id": DatabaseId::mint().as_str(),
                "capability": DatabaseCapability::ReadWrite.as_wire(),
            }]})
            .to_string(),
        ),
        (route(endpoints::CONTROL_APP_ENV, app), ENV.to_owned()),
    ]
}

/// The host's own creator factory over `provider`, preparing apps into a
/// bounded cache as the worker host does.
fn prepared_apps(
    host: &Host,
    provider: ProductionResources,
) -> PreparedApps<WorkflowCreatorFactory<ProductionResources>> {
    let client = WorkerCoordinator::new(
        "http://127.0.0.1:1",
        host.auth.clone(),
        ClientOptions::default(),
    )
    .expect("a client against a loopback origin");
    let objects = PayloadObjects::open(StorageStore::open(&host.storage).expect("object storage"))
        .expect("payload objects");
    let limits = TaskPayloadLimits::default();
    let workflows = RemoteWorkflows::new(client.clone(), objects, limits.max_payload_bytes)
        .expect("the request path's registry");
    let factory =
        WorkflowCreatorFactory::new(provider, client, workflows, limits, OPERATION_TIMEOUT)
            .expect("the host's creator factory");
    PreparedApps::new(
        factory,
        // Every app is listed: the feed is not what these cases measure.
        Rc::new(|_: &AppId| true),
        PreparedOptions {
            capacity: 1,
            operation_timeout: OPERATION_TIMEOUT,
        },
    )
    .expect("prepared app bounds")
}

/// Preparing an app supplies its key, bindings and environment from Control,
/// and the last holder's drop withdraws all three.
///
/// The control is the drop that is not the last: another holder of the same
/// app keeps everything supplied.
#[compio::test]
async fn a_prepared_app_holds_what_control_supplied_until_its_last_holder_drops() {
    let host = Host::new();
    let app = AppId::mint();
    let control = ControlPlane::serving(4, served(&app, [5; 32]));
    let provider = host.provider(&control.base_url);
    assert_eq!(
        host.supplied(&app),
        [false; 3],
        "the premise: nothing supplied yet"
    );

    let resources = provider.resolve(&app).await.expect("the app is prepared");
    assert_eq!(host.supplied(&app), [true; 3]);
    assert_eq!(
        control.served(),
        vec![
            route(endpoints::CONTROL_APP, &app),
            route(endpoints::CONTROL_APP_DATA_KEY, &app),
            route(endpoints::CONTROL_APP_BINDINGS, &app),
            route(endpoints::CONTROL_APP_ENV, &app),
        ]
    );

    let other = host.residency.reside(app.clone());
    drop(resources);
    assert_eq!(
        host.supplied(&app),
        [true; 3],
        "another holder keeps the app"
    );
    drop(other);
    assert_eq!(
        host.supplied(&app),
        [false; 3],
        "the last holder's drop withdraws the key, the bindings and the environment"
    );
}

/// An execution running from a prepared app keeps reading its app's encrypted
/// data after every other holder is gone: the request path's isolate and the
/// prepared-app cache itself. The same read refuses once the execution ends.
#[compio::test]
async fn an_execution_keeps_reading_encrypted_data_after_the_other_holders_drop() {
    let host = Host::new();
    let app = AppId::mint();
    let root = [6; 32];
    *host.versions.write().expect("versions") = Some(VersionMap::from([(app.clone(), version(1))]));
    let control = ControlPlane::serving(4, served(&app, root));
    let prepared = prepared_apps(&host, host.provider(&control.base_url));

    let execution = prepared
        .get_or_prepare(&app, OPERATION_TIMEOUT)
        .await
        .expect("the app is prepared for its first delivery");
    let request_path = host.residency.reside(app.clone());
    assert_eq!(
        control.served().len(),
        4,
        "the premise: prepared from Control"
    );

    drop(request_path);
    assert_eq!(host.read(&app, root).await.as_deref(), Ok(&b"held"[..]));
    drop(prepared);
    assert_eq!(
        host.read(&app, root).await.as_deref(),
        Ok(&b"held"[..]),
        "the execution still holds the prepared app after its cache is gone"
    );

    drop(execution);
    let refused = host
        .read(&app, root)
        .await
        .expect_err("with the execution gone nothing holds the app");
    assert!(refused.contains("No project encryption key"), "{refused}");
    assert_eq!(host.supplied(&app), [false; 3]);
}

/// A preparation takes its residency BEFORE it asks whether the app's material
/// is already supplied, so another holder dropping between that question and
/// the material's use cannot withdraw it.
///
/// The other holder supplied everything, so the preparation skips every fetch
/// but the version read. Control holds that read's answer while the other
/// holder drops: the drop lands after the preparation took its residency and
/// before it checks anything, and it must withdraw nothing.
#[compio::test]
async fn a_residency_taken_before_the_supply_checks_cannot_be_withdrawn_under_them() {
    let host = Host::new();
    let app = AppId::mint();
    let root = [7; 32];
    let other = host.residency.reside(app.clone());
    host.db
        .project_keys()
        .supply(app.as_str(), ProjectId::mint().as_str(), root)
        .expect("the other holder's key");
    crate::cache::fixture::bind_app(&host.db, &app);
    sync::put_env_from_json(&host.envs, app.clone(), ENV, 1).expect("the other holder's env");

    let version_route = route(endpoints::CONTROL_APP, &app);
    let (control, mut gate) = ControlPlane::gated(1, served(&app, root), version_route.clone());
    let provider = host.provider(&control.base_url);
    let mut preparing = Box::pin(provider.resolve(&app));
    match futures::future::select(Box::pin(gate.arrived()), preparing.as_mut()).await {
        Either::Left(((), _)) => {}
        Either::Right((outcome, _)) => {
            panic!("the preparation finished before Control answered: {outcome:?}")
        }
    }
    assert_eq!(
        host.residency.holders(&app),
        2,
        "the preparation already holds the app"
    );
    drop(other);
    gate.release();
    let resources = preparing.await.expect("the preparation completes");

    assert_eq!(
        control.served(),
        vec![version_route],
        "nothing was withdrawn, so nothing was fetched again"
    );
    assert_eq!(host.supplied(&app), [true; 3]);
    assert_eq!(host.read(&app, root).await.as_deref(), Ok(&b"held"[..]));
    drop(resources);
    assert_eq!(host.supplied(&app), [false; 3]);
}
