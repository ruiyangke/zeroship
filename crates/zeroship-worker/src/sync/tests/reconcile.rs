use super::fixture::{write_blob, DeployedApp};
use super::*;
use crate::control_fixture::{route, ControlPlane};
use crate::worker_fixture::dispatch_frame;
use sha2::{Digest, Sha256};

/// A deployed app on a worker whose kernel installed a database service, so
/// the app has a resolved binding and a store to re-resolve into.
///
/// The database URL is never connected to: these cases measure the binding
/// STORE, and no session is opened against the cluster it names.
async fn deployed_with_database() -> DeployedApp {
    let kernel = crate::cache::KernelConfig {
        workflows: zeroship_workflow::service::runner::ready::ReadyApps::default(),
        db_service: Some(crate::cache::fixture::database_service(
            "postgresql://fixture:fixture@localhost/unused",
        )),
        kv_store: None,
        storage_backend: None,
        meter: Arc::new(zeroship_metering::Meter::new()),
    };
    DeployedApp::with_worker(crate::worker_fixture::Worker::with_kernel(10, kernel)).await
}

/// Control's binding response for one resolved edge at a stated epoch, in the
/// shape `zeroship_control::internal::get_app_bindings` serves.
fn binding_body(
    resolved: &zeroship_data_orm::resolved_bindings::ResolvedBinding,
    epoch: u32,
) -> String {
    serde_json::json!({
        "bindings": [{
            "binding_id": resolved.binding.as_str(),
            "database_id": resolved.database.as_str(),
            "schema_epoch": epoch,
            // The capability the response carries is the one the store already
            // holds: a rotation advances the epoch and nothing else, and
            // `supply` refuses a reading that disagrees about the capability.
            "capability": resolved.capability.as_wire(),
        }]
    })
    .to_string()
}

/// A reload installs the epoch control now serves, because the isolate it
/// builds captures the binding its sessions narrow with WHILE it builds.
///
/// The whole point of the fifth reload signal: nothing else about this app
/// changed - same deploy, same env, same limits, same policy - and a worker
/// that did not compare the epoch would leave this isolate composing the role
/// the apply retired, with every session it opened refused at
/// `SET LOCAL ROLE`.
#[compio::test]
async fn a_rotated_epoch_reloads_the_app_onto_the_role_control_now_serves() {
    let mut case = deployed_with_database().await;
    let installed = case.resolved_binding();
    let rotated = installed.epoch + 1;
    let bindings_route = route(endpoints::CONTROL_APP_BINDINGS, &case.worker.app_id);
    let control = ControlPlane::serving(
        1,
        vec![(bindings_route.clone(), binding_body(&installed, rotated))],
    );
    case.control_url = control.base_url.clone();
    case.version.binding_epochs =
        std::collections::BTreeMap::from([(installed.database.clone(), rotated)]);

    case.reconcile().await;

    assert_eq!(
        control.served(),
        vec![bindings_route],
        "the reload re-resolved the binding at the route control declares"
    );
    assert_eq!(
        case.installed_epochs(),
        case.version.binding_epochs,
        "the store holds the epoch control serves, so the isolate built above \
         narrows to the role the apply minted"
    );
    assert_eq!(
        cache::get_loaded_meta(&case.worker.app_id)
            .expect("the reload recorded what it built against")
            .binding_epochs,
        case.version.binding_epochs,
        "and the isolate records it, so the next cycle does not reload again"
    );
    // The app is still serving: the reload replaced the isolate rather than
    // dropping it.
    assert_eq!(case.body().await, b"var-old|sec-old|var-old");
}

/// An app whose epoch did NOT move makes no binding call and reloads nothing.
///
/// The rejection control for the case above. Without it that one would pass
/// over a reconcile that re-resolved on every tick - a control call per app
/// per poll, and a store advancing under isolates nothing was replacing.
#[compio::test]
async fn a_reconcile_with_no_change_re_resolves_nothing() {
    let mut case = deployed_with_database().await;
    let installed = case.resolved_binding();
    // A control plane that expects NO request. One would be recorded.
    let control = ControlPlane::serving(0, Vec::new());
    case.control_url = control.base_url.clone();

    case.reconcile().await;

    assert_eq!(
        control.served(),
        Vec::<String>::new(),
        "an app whose epoch did not move resolves nothing"
    );
    assert_eq!(
        case.installed_epochs(),
        std::collections::BTreeMap::from([(installed.database, installed.epoch)]),
        "and the store is left exactly as the app's isolate was built with"
    );
}

/// A reload whose binding cannot be re-resolved keeps the previous isolate.
///
/// The same discipline as the bundle and env failures beside it: an isolate
/// built on a binding control would not serve is worse than the one already
/// running, and the app comes back on the next cycle because nothing recorded
/// the reload as done.
#[compio::test]
async fn a_reload_that_cannot_re_resolve_the_binding_keeps_the_previous_app() {
    let mut case = deployed_with_database().await;
    let installed = case.resolved_binding();
    let previous = case.version.deploy_hash.clone();
    let replacement = br#"export default { fetch() { return new Response("replacement"); } }"#;
    let hash = write_blob(&case.worker.config.blob_store, replacement).await;
    case.version.deploy_hash = Some("replacement-deploy".into());
    case.version
        .manifest
        .as_mut()
        .unwrap()
        .worker
        .as_mut()
        .unwrap()
        .modules
        .insert("index.js".into(), hash);
    case.version.binding_epochs =
        std::collections::BTreeMap::from([(installed.database.clone(), installed.epoch + 1)]);
    // `Worker::new`'s control address: a port nothing listens on.
    assert_eq!(case.control_url, "http://127.0.0.1:1");

    case.reconcile().await;

    assert_eq!(
        case.body().await,
        b"var-old|sec-old|var-old",
        "the previous isolate keeps serving"
    );
    assert_eq!(
        cache::get_loaded_meta(&case.worker.app_id)
            .unwrap()
            .deploy_hash,
        previous,
        "and nothing recorded the reload, so the next cycle retries it"
    );
    assert_eq!(
        case.installed_epochs(),
        std::collections::BTreeMap::from([(installed.database, installed.epoch)]),
        "a failed resolution installs nothing rather than unbinding the app"
    );

    // The control differing in ONE variable: with a control plane that serves
    // the binding, the same reload completes.
    let installed = case.resolved_binding();
    let bindings_route = route(endpoints::CONTROL_APP_BINDINGS, &case.worker.app_id);
    let control = ControlPlane::serving(
        1,
        vec![(
            bindings_route.clone(),
            binding_body(&installed, installed.epoch + 1),
        )],
    );
    case.control_url = control.base_url.clone();
    case.reconcile().await;
    assert_eq!(control.served(), vec![bindings_route]);
    assert_eq!(case.body().await, b"replacement");
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
