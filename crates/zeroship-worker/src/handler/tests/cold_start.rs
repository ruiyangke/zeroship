//! What a COLD start resolves before it builds an isolate.
//!
//! A cold start on this thread is not a cold PROCESS. The binding store is
//! process-wide and outlives every isolate in it, so an app another thread
//! resolved before a bind or an unbind is still in the store at the set that
//! change left behind - and the resolution inside `sync::fetch_app_env` is
//! guarded on the app being UNRESOLVED, so it would not fire for exactly that
//! app. An isolate captures the bindings its sessions narrow with while it
//! builds, so a cold start that skipped the comparison would build one on the
//! stale set and record the set control reports, which leaves
//! `sync::needs_reload` with nothing to notice and the app serving the wrong
//! binding topology for the life of the process.

use super::*;
use crate::control_fixture::{route, ControlPlane};
use zeroship_core::service_identity::endpoints;
use zeroship_core::types::{AppNetPolicy, AppVersionInfo, LiveBinding};
use zeroship_core::{BindingId, DatabaseId};

/// A kernel that installed a database service, so the app has a resolved
/// binding and a store to compare against.
fn database_kernel() -> crate::cache::KernelConfig {
    crate::cache::KernelConfig {
        workflows: None,
        db_service: Some(crate::cache::fixture::database_service(
            "postgresql://fixture:fixture@localhost/unused",
        )),
        kv_store: None,
        storage_backend: None,
        meter: Arc::new(zeroship_metering::Meter::new()),
        residency: None,
    }
}

fn worker_with_database() -> Worker {
    Worker::with_kernel(10, database_kernel())
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

/// The version feed entry control serves for this app, carrying the live
/// binding set the case wants it to report.
async fn version_body(worker: &Worker, live_bindings: &[(&DatabaseId, &LiveBinding)]) -> String {
    version_of(
        worker,
        br#"export default { fetch() { return new Response("cold"); } }"#,
        live_bindings,
    )
    .await
}

/// The same entry for a deployment of `source`.
async fn version_of(
    worker: &Worker,
    source: &[u8],
    live_bindings: &[(&DatabaseId, &LiveBinding)],
) -> String {
    use sha2::{Digest, Sha256};
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
        live_bindings: live_bindings
            .iter()
            .map(|(database, live)| ((*database).clone(), (*live).clone()))
            .collect(),
    })
    .expect("a version feed entry serializes")
}

/// A cold start whose store disagrees with the set control reports re-resolves
/// the bindings, BEFORE it reads the environment.
///
/// The rejection control is the same cold start with the sets agreeing, which
/// must address no binding route at all - without it this would pass over a
/// worker that re-resolved unconditionally, and the order assertion would still
/// hold.
#[compio::test]
async fn a_cold_start_re_resolves_a_binding_set_the_store_disagrees_with() {
    let worker = worker_with_database();
    let bindings = crate::cache::app_bindings().expect("the kernel installed a database service");
    let installed = bindings.live_bindings_for(worker.app_id.as_str());
    let [(database, live)] = installed.iter().collect::<Vec<_>>()[..] else {
        panic!("this case's app holds exactly one binding");
    };
    let (database, live) = (database.clone(), live.clone());

    // Control reports a SECOND database, and serves only the version read: the
    // call after it is the one under test, and its failure is what names it.
    let gained = DatabaseId::mint();
    let gained_live = LiveBinding {
        binding: BindingId::mint(),
        capability: live.capability,
    };
    let control = ControlPlane::serving(
        1,
        vec![(
            route(endpoints::CONTROL_APP, &worker.app_id),
            version_body(&worker, &[(&database, &live), (&gained, &gained_live)]).await,
        )],
    );
    let mut config = addressing(&worker, &control.base_url);
    let error = super::super::load_on_demand(&config, &worker.envs, &worker.app_id)
        .await
        .expect_err("the control plane serves nothing after the version read");
    assert!(
        error.starts_with("binding resolution failed"),
        "a cold start resolves the binding set before it reads the environment, \
         so the binding read is what fails here: {error}"
    );
    assert_eq!(
        control.served(),
        vec![route(endpoints::CONTROL_APP, &worker.app_id)],
        "and it got that far: the version read, then the call that failed"
    );
    assert!(
        crate::cache::get_runtime(&worker.app_id).is_none(),
        "nothing was built on a binding set control would not serve"
    );
    assert_eq!(
        bindings.live_bindings_for(worker.app_id.as_str()),
        installed,
        "a failed resolution installs nothing rather than unbinding the app"
    );

    // THE CONTROL, one variable changed: control reports the set the store
    // already holds, and the cold start asks for no binding.
    let control = ControlPlane::serving(
        1,
        vec![(
            route(endpoints::CONTROL_APP, &worker.app_id),
            version_body(&worker, &[(&database, &live)]).await,
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

/// Another holder of the app, with the app's project key supplied beside the
/// binding and environment the fixture resolved: what the workflow host's
/// prepared app leaves supplied for a cold start on an HTTP thread to find.
fn held_elsewhere(worker: &Worker) -> crate::residency::Residency {
    let other = worker
        .residency
        .as_ref()
        .expect("the case accounts for credentials")
        .reside(worker.app_id.clone());
    crate::cache::project_keys()
        .expect("the kernel installed a database service")
        .supply(
            worker.app_id.as_str(),
            zeroship_core::ProjectId::mint().as_str(),
            [3; 32],
        )
        .expect("the app's project key");
    other
}

/// Which of the app's credentials this worker holds: key, bindings, env.
fn supplied(worker: &Worker) -> [bool; 3] {
    let app = worker.app_id.as_str();
    [
        crate::cache::project_keys().unwrap().is_bound(app).unwrap(),
        crate::cache::app_bindings().unwrap().is_bound(app).unwrap(),
        crate::sync::get_env(&worker.envs, &worker.app_id).is_some(),
    ]
}

/// The binding set the store holds for this app, as control's feed reports it.
fn installed(worker: &Worker) -> (DatabaseId, LiveBinding) {
    let bindings = crate::cache::app_bindings().expect("the kernel installed a database service");
    let installed = bindings.live_bindings_for(worker.app_id.as_str());
    let [(database, live)] = installed.iter().collect::<Vec<_>>()[..] else {
        panic!("this case's app holds exactly one binding");
    };
    (database.clone(), live.clone())
}

/// A cold start that fails to build its isolate leaves what another holder of
/// the app holds: the environment, the key and the bindings stay supplied.
///
/// The control is the same failed cold start as the app's only holder, which
/// withdraws everything it supplied: a failure leaves nothing behind that
/// nothing holds.
#[compio::test]
async fn a_failed_cold_start_withdraws_only_what_nothing_else_holds() {
    let worker = Worker::holding(10, database_kernel());
    let other = held_elsewhere(&worker);
    let (database, live) = installed(&worker);
    let throwing = br#"throw new Error("startup rejected"); export default {};"#;
    let version = version_of(&worker, throwing, &[(&database, &live)]).await;
    let env = r#"{"vars":{},"secrets":{},"expose":[]}"#;
    let control = ControlPlane::serving(
        2,
        vec![
            (route(endpoints::CONTROL_APP, &worker.app_id), version.clone()),
            (route(endpoints::CONTROL_APP_ENV, &worker.app_id), env.to_owned()),
        ],
    );
    let error = super::super::load_on_demand(
        &addressing(&worker, &control.base_url),
        &worker.envs,
        &worker.app_id,
    )
    .await
    .expect_err("the deployment throws while it starts");
    assert!(error.starts_with("failed to load bundle"), "{error}");
    assert_eq!(
        control.served(),
        vec![
            route(endpoints::CONTROL_APP, &worker.app_id),
            route(endpoints::CONTROL_APP_ENV, &worker.app_id),
        ]
    );
    assert_eq!(
        supplied(&worker),
        [true; 3],
        "another holder still holds the app, so the failure withdraws nothing"
    );

    drop(other);
    assert_eq!(supplied(&worker), [false; 3], "the premise of the control");
    let control = ControlPlane::serving(
        4,
        vec![
            (route(endpoints::CONTROL_APP, &worker.app_id), version),
            (
                route(endpoints::CONTROL_APP_BINDINGS, &worker.app_id),
                serde_json::json!({"bindings": [{
                    "binding_id": live.binding.as_str(),
                    "database_id": database.as_str(),
                    "capability": live.capability.as_wire(),
                }]})
                .to_string(),
            ),
            (
                route(endpoints::CONTROL_APP_DATA_KEY, &worker.app_id),
                serde_json::to_string(&zeroship_core::project_data_key::ProjectDataKey::new(
                    zeroship_core::ProjectId::mint(),
                    [3; 32],
                ))
                .expect("project key"),
            ),
            (route(endpoints::CONTROL_APP_ENV, &worker.app_id), env.to_owned()),
        ],
    );
    super::super::load_on_demand(
        &addressing(&worker, &control.base_url),
        &worker.envs,
        &worker.app_id,
    )
    .await
    .expect_err("the deployment still throws while it starts");
    assert_eq!(control.served().len(), 4, "everything was supplied again first");
    assert_eq!(
        supplied(&worker),
        [false; 3],
        "the failed cold start was the only holder, so it withdraws what it supplied"
    );
}

/// A cold start holds its app BEFORE it asks what is already supplied, so
/// another holder dropping between that question and the isolate being built
/// cannot withdraw what the cold start found.
///
/// The other holder supplied everything, so the cold start fetches only the
/// version and the environment. Control holds the version read's answer while
/// the other holder drops: the drop lands after the cold start took its hold
/// and before it compared bindings or asked whether the key is bound.
#[compio::test]
async fn a_cold_start_holds_its_app_before_asking_what_is_supplied() {
    let worker = Worker::holding(10, database_kernel());
    let other = held_elsewhere(&worker);
    let registry = worker.residency.clone().expect("the case accounts for credentials");
    let (database, live) = installed(&worker);
    let version_route = route(endpoints::CONTROL_APP, &worker.app_id);
    let env_route = route(endpoints::CONTROL_APP_ENV, &worker.app_id);
    let (control, mut gate) = ControlPlane::gated(
        2,
        vec![
            (
                version_route.clone(),
                version_body(&worker, &[(&database, &live)]).await,
            ),
            (
                env_route.clone(),
                r#"{"vars":{},"secrets":{},"expose":[]}"#.to_owned(),
            ),
        ],
        version_route.clone(),
    );
    let config = addressing(&worker, &control.base_url);
    {
        let mut loading = Box::pin(super::super::load_on_demand(
            &config,
            &worker.envs,
            &worker.app_id,
        ));
        match futures::future::select(Box::pin(gate.arrived()), loading.as_mut()).await {
            futures::future::Either::Left(((), _)) => {}
            futures::future::Either::Right((outcome, _)) => {
                panic!("the cold start finished before Control answered: {outcome:?}")
            }
        }
        assert_eq!(registry.holders(&worker.app_id), 2, "the cold start already holds the app");
        drop(other);
        gate.release();
        loading.await.expect("the cold start completes");
    }
    assert_eq!(
        control.served(),
        vec![version_route, env_route],
        "nothing was withdrawn, so neither the key nor the bindings were fetched again"
    );
    assert_eq!(supplied(&worker), [true; 3]);
    assert!(crate::cache::get_runtime(&worker.app_id).is_some());
    assert_eq!(registry.holders(&worker.app_id), 1, "the isolate holds the app");
}
