use super::fixture::{binding_store, write_blob, DeployedApp};
use super::*;
use crate::control_fixture::ControlPlane;
use crate::worker_fixture::dispatch_frame;
use sha2::{Digest, Sha256};

/// A deployed app on a worker whose kernel installed a database service, so
/// the app has a resolved binding and a store to re-resolve into.
///
/// The database URL is never connected to: these cases measure the binding
/// STORE, and no session is opened against the cluster it names.
async fn deployed_with_database() -> DeployedApp {
    let kernel = crate::cache::KernelConfig {
        workflows: zeroship_workflow_runner::ready::ReadyApps::default(),
        db_service: Some(crate::cache::fixture::database_service(
            "postgresql://fixture:fixture@localhost/unused",
        )),
        kv_store: None,
        storage_backend: None,
        meter: Arc::new(zeroship_metering::Meter::new()),
    };
    DeployedApp::with_worker(crate::worker_fixture::Worker::with_kernel(10, kernel)).await
}

/// A reconcile with nothing changed makes no control call and leaves the
/// resolved binding exactly as the isolate was built with.
///
/// The binding follows the ISOLATE: an isolate captures the binding its
/// sessions narrow with while it builds, so a reconcile that re-read the
/// binding on every tick would be a control call per app per poll and a store
/// moving under isolates nothing was replacing. Its control is the store read,
/// which must still hold the edge - without it this would pass over a
/// reconcile that had unbound the app.
#[compio::test]
async fn a_reconcile_with_no_change_makes_no_control_call() {
    let mut case = deployed_with_database().await;
    let installed = case.resolved_binding();
    // A control plane that expects NO request. One would be recorded.
    let control = ControlPlane::serving(0, Vec::new());
    case.control_url = control.base_url.clone();

    case.reconcile().await;

    assert_eq!(
        control.served(),
        Vec::<String>::new(),
        "an app whose version feed entry did not move resolves nothing"
    );
    let after = binding_store().bindings_for(case.worker.app_id.as_str(), "d");
    let [edge] = after.as_slice() else {
        panic!("the app still holds exactly one binding");
    };
    assert_eq!(
        edge.database(),
        Some(&installed.database),
        "and the store is left exactly as the app's isolate was built with"
    );
}

#[compio::test]
async fn reconcile_swaps_isolate_on_env_only_rotation() {
    let mut case = DeployedApp::new().await;
    assert_eq!(case.body().await, b"var-old|sec-old|var-old");

    // SharedEnvs is already refreshed, as it would be by another worker thread.
    // The running isolate must still be replaced to update its materialized env.
    case.set_environment(2, "var-new", "sec-new");
    assert_eq!(case.body().await, b"var-old|sec-old|var-old");
    case.version.env_version = 2;
    case.reconcile().await;

    assert_eq!(case.body().await, b"var-new|sec-new|var-new");
    assert_eq!(
        cache::get_loaded_meta(&case.worker.app_id)
            .unwrap()
            .env_version,
        2
    );
}

#[compio::test]
async fn reconcile_removes_deleted_app_runtime_metadata_and_environment() {
    let case = DeployedApp::new().await;
    assert_eq!(case.body().await, b"var-old|sec-old|var-old");
    reconcile_once(&case.worker.config, &VersionMap::new(), &case.worker.envs)
        .await
        .unwrap();

    assert!(cache::get_runtime(&case.worker.app_id).is_none());
    assert!(cache::get_loaded_meta(&case.worker.app_id).is_none());
    assert!(get_env(&case.worker.envs, &case.worker.app_id).is_none());
    let response = case
        .worker
        .dispatch(dispatch_frame("GET", "http://example.test/", b""))
        .await;
    assert_eq!(response.status, ntex::http::StatusCode::SERVICE_UNAVAILABLE);
}

#[compio::test]
async fn reconcile_evicts_a_withdrawn_deploy_without_serving_stale_code() {
    let mut case = DeployedApp::new().await;
    assert_eq!(case.body().await, b"var-old|sec-old|var-old");
    case.version.deploy_hash = None;
    case.version.manifest = None;
    case.reconcile().await;

    assert!(cache::get_runtime(&case.worker.app_id).is_none());
    assert!(cache::get_loaded_meta(&case.worker.app_id).is_none());
    assert!(get_env(&case.worker.envs, &case.worker.app_id).is_some());
    let response = case
        .worker
        .dispatch(dispatch_frame("GET", "http://example.test/", b""))
        .await;
    assert_eq!(response.status, ntex::http::StatusCode::SERVICE_UNAVAILABLE);
}

#[compio::test]
async fn reconcile_keeps_the_previous_app_until_replacement_bytes_are_available() {
    let mut case = DeployedApp::new().await;
    let previous = case.version.deploy_hash.clone();
    assert_eq!(case.body().await, b"var-old|sec-old|var-old");
    let replacement = br#"export default { fetch() { return new Response("replacement"); } }"#;
    let hash = hex::encode(Sha256::digest(replacement));
    case.version.deploy_hash = Some("replacement-deploy".into());
    case.version
        .manifest
        .as_mut()
        .unwrap()
        .worker
        .as_mut()
        .unwrap()
        .modules
        .insert("index.js".into(), hash.clone());

    case.reconcile().await;
    assert_eq!(case.body().await, b"var-old|sec-old|var-old");
    assert_eq!(
        cache::get_loaded_meta(&case.worker.app_id)
            .unwrap()
            .deploy_hash,
        previous
    );

    assert_eq!(
        write_blob(&case.worker.config.blob_store, replacement).await,
        hash
    );
    case.reconcile().await;
    assert_eq!(case.body().await, b"replacement");
    assert_eq!(
        cache::get_loaded_meta(&case.worker.app_id)
            .unwrap()
            .deploy_hash,
        case.version.deploy_hash
    );
}
