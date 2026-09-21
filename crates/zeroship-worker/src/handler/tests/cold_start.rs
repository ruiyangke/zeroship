//! What a COLD start resolves before it builds an isolate.
//!
//! A cold start on this thread is not a cold PROCESS. The binding store is
//! process-wide and outlives every isolate in it, so an app another thread
//! resolved before an apply rotated its role is still in the store at the edge
//! that apply retired - and the resolution inside `sync::fetch_app_env` is
//! guarded on the app being UNRESOLVED, so it would not fire for exactly that
//! app. An isolate captures the binding its sessions narrow with while it
//! builds, so a cold start that skipped the comparison would build one on the
//! retired role and record the epoch control reports, which leaves
//! `sync::needs_reload` with nothing to notice and the app fenced for good.

use super::*;
use crate::control_fixture::{route, ControlPlane};
use zeroship_core::service_identity::endpoints;
use zeroship_core::types::{AppNetPolicy, AppVersionInfo};

/// A worker whose kernel installed a database service, so the app has a
/// resolved binding and a store to compare against.
fn worker_with_database() -> Worker {
    Worker::with_kernel(
        10,
        crate::cache::KernelConfig {
            workflows: zeroship_workflow::service::runner::ready::ReadyApps::default(),
            db_service: Some(crate::cache::fixture::database_service(
                "postgresql://fixture:fixture@localhost/unused",
            )),
            kv_store: None,
            storage_backend: None,
            meter: Arc::new(zeroship_metering::Meter::new()),
        },
    )
}

/// This worker's configuration, addressing the control plane a case started.
fn addressing(worker: &Worker, control_url: &str) -> crate::WorkerConfig {
    crate::WorkerConfig {
        service_auth: worker.config.service_auth.clone(),
        control_url: control_url.to_owned(),
        control_key: worker.config.control_key.clone(),
        kv_store: None,
        storage_backend: None,
        max_isolates: worker.config.max_isolates,
        poll_interval_secs: worker.config.poll_interval_secs,
        shutdown_timeout_secs: worker.config.shutdown_timeout_secs,
        blob_store: worker.config.blob_store.clone(),
    }
}

/// The version feed entry control serves for this app, carrying the epochs
/// the case wants it to report.
async fn version_body(
    worker: &Worker,
    binding_epochs: &[(&zeroship_core::DatabaseId, u32)],
) -> String {
    use sha2::{Digest, Sha256};
    let source = br#"export default { fetch() { return new Response("cold"); } }"#;
    let hash = hex::encode(Sha256::digest(source));
    worker
        .config
        .blob_store
        .put_blob(&hash, source)
        .await
        .expect("store the deployment blob");
    let manifest: Manifest = serde_json::from_value(serde_json::json!({
        "version": 1,
        "worker": { "entry": "index.js", "modules": { "index.js": hash } },
    }))
    .expect("deployment manifest");
    serde_json::to_string(&AppVersionInfo {
        deploy_hash: Some("cold-deploy".into()),
        plan_id: "starter".into(),
        runtime: AppRuntimeLimits::default(),
        env_version: 1,
        manifest: Some(manifest),
        net_policy: AppNetPolicy::default(),
        binding_epochs: binding_epochs
            .iter()
            .map(|(database, epoch)| ((*database).clone(), *epoch))
            .collect(),
    })
    .expect("a version feed entry serializes")
}

/// A cold start whose store disagrees with the epochs control reports
/// re-resolves the binding, BEFORE it reads the environment.
///
/// The rejection control is the same cold start with the epochs agreeing,
/// which must address no binding route at all - without it this would pass
/// over a worker that re-resolved unconditionally, and the order assertion
/// would still hold.
#[compio::test]
async fn a_cold_start_re_resolves_a_binding_the_store_holds_at_another_epoch() {
    let worker = worker_with_database();
    let bindings = crate::cache::app_bindings().expect("the kernel installed a database service");
    let installed = bindings.epochs_for(worker.app_id.as_str());
    let [(database, epoch)] = installed.iter().collect::<Vec<_>>()[..] else {
        panic!("this case's app holds exactly one binding");
    };
    let (database, epoch) = (database.clone(), *epoch);

    // Control reports the NEXT epoch, and serves only the version read: the
    // call after it is the one under test, and its failure is what names it.
    let control = ControlPlane::serving(
        1,
        vec![(
            route(endpoints::CONTROL_APP, &worker.app_id),
            version_body(&worker, &[(&database, epoch + 1)]).await,
        )],
    );
    let mut config = addressing(&worker, &control.base_url);
    let error = super::super::load_on_demand(&config, &worker.envs, &worker.app_id)
        .await
        .expect_err("the control plane serves nothing after the version read");
    assert!(
        error.starts_with("binding resolution failed"),
        "a cold start resolves the binding before it reads the environment, so \
         the binding read is what fails here: {error}"
    );
    assert_eq!(
        control.served(),
        vec![route(endpoints::CONTROL_APP, &worker.app_id)],
        "and it got that far: the version read, then the call that failed"
    );
    assert!(
        crate::cache::get_runtime(&worker.app_id).is_none(),
        "nothing was built on a binding control would not serve"
    );
    assert_eq!(
        bindings.epochs_for(worker.app_id.as_str()),
        installed,
        "a failed resolution installs nothing rather than unbinding the app"
    );

    // THE CONTROL, one variable changed: control reports the epoch the store
    // already holds, and the cold start asks for no binding.
    let control = ControlPlane::serving(
        1,
        vec![(
            route(endpoints::CONTROL_APP, &worker.app_id),
            version_body(&worker, &[(&database, epoch)]).await,
        )],
    );
    config = addressing(&worker, &control.base_url);
    let error = super::super::load_on_demand(&config, &worker.envs, &worker.app_id)
        .await
        .expect_err("this control plane serves nothing after the version read either");
    assert!(
        !error.starts_with("binding resolution failed"),
        "a store that already agrees with control has nothing to resolve: {error}"
    );
    assert_eq!(
        control.served(),
        vec![route(endpoints::CONTROL_APP, &worker.app_id)]
    );
}
