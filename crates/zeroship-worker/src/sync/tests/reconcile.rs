use super::fixture::{write_blob, DeployedApp};
use super::*;
use crate::worker_fixture::dispatch_frame;
use sha2::{Digest, Sha256};

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
