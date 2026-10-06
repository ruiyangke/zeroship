use super::fixture::{binding_store, write_blob, DeployedApp};
use super::*;
use crate::control_fixture::{route, ControlPlane};
use crate::worker_fixture::dispatch_frame;
use sha2::{Digest, Sha256};
use zeroship_core::database_role::DatabaseCapability;
use zeroship_core::types::LiveBinding;
use zeroship_core::{BindingId, DatabaseId};
use zeroship_data_orm::resolved_bindings::ResolvedBinding;

/// A deployed app on a worker whose kernel installed a database service, so
/// the app has a resolved binding and a store to re-resolve into.
///
/// The database URL is never connected to: these cases measure the binding
/// STORE, and no session is opened against the cluster it names.
async fn deployed_with_database() -> DeployedApp {
    let kernel = crate::cache::KernelConfig {
        workflows: None,
        db_service: Some(crate::cache::fixture::database_service(
            "postgresql://fixture:fixture@localhost/unused",
        )),
        kv_store: None,
        storage_backend: None,
        meter: Arc::new(zeroship_metering::Meter::new()),
        residency: None,
    };
    DeployedApp::with_worker(crate::worker_fixture::Worker::with_kernel(10, kernel)).await
}

/// Control's binding response for a resolved set, in the shape
/// `zeroship_control::internal::get_app_bindings` serves: one object per live
/// binding under a `bindings` array, with the capability written through the
/// one codec control serves it with.
fn binding_body(resolved: &[ResolvedBinding]) -> String {
    serde_json::json!({
        "bindings": resolved
            .iter()
            .map(|edge| serde_json::json!({
                "binding_id": edge.binding.as_str(),
                "database_id": edge.database.as_str(),
                "capability": edge.capability.as_wire(),
            }))
            .collect::<Vec<_>>()
    })
    .to_string()
}

/// The live binding set a resolved set projects to - what control's version
/// feed reports for an app whose store holds exactly these edges.
fn live_bindings(
    resolved: &[ResolvedBinding],
) -> std::collections::BTreeMap<DatabaseId, LiveBinding> {
    resolved
        .iter()
        .map(|edge| {
            (
                edge.database.clone(),
                LiveBinding {
                    binding: edge.binding.clone(),
                    capability: edge.capability,
                },
            )
        })
        .collect()
}

/// An app BOUND to a new database reloads onto it, because the isolate it
/// replaces was built with no handle for that database at all.
///
/// The whole point of the binding reload signal: nothing else about this app
/// changed - same deploy, same env, same limits, same policy - and a worker
/// that did not compare the set would leave this app unable to reach the
/// database control has bound it to until the worker PROCESS restarted, because
/// the `is_bound` guard in `fetch_app_env_supplying` short-circuits for an app
/// some isolate already resolved.
#[compio::test]
async fn an_app_bound_to_a_new_database_reloads_onto_it() {
    let mut case = deployed_with_database().await;
    let installed = case.resolved_binding();
    let gained = ResolvedBinding {
        database: DatabaseId::mint(),
        binding: BindingId::mint(),
        capability: DatabaseCapability::ReadOnly,
    };
    assert_ne!(
        gained.database, installed.database,
        "the premise: this is a SECOND database, not the one already bound"
    );
    assert_eq!(
        case.installed_bindings().len(),
        1,
        "the premise: the store starts non-empty, so the assertions below are \
         not passing over an app that binds nothing"
    );

    let served = vec![installed.clone(), gained.clone()];
    let bindings_route = route(endpoints::CONTROL_APP_BINDINGS, &case.worker.app_id);
    let control = ControlPlane::serving(1, vec![(bindings_route.clone(), binding_body(&served))]);
    case.control_url = control.base_url.clone();
    case.version.live_bindings = live_bindings(&served);

    case.reconcile().await;

    assert_eq!(
        control.served(),
        vec![bindings_route],
        "the reload re-resolved the binding set at the route control declares"
    );
    assert_eq!(
        case.installed_bindings(),
        case.version.live_bindings,
        "the store holds the set control serves, so the isolate built above \
         has a handle for the database it was just bound to"
    );
    assert_eq!(
        case.resolved_edges()
            .get(&gained.database)
            .map(|edge| edge.binding.clone()),
        Some(gained.binding),
        "and it narrows with the EDGE control resolved, which no worker composes"
    );
    assert_eq!(
        cache::get_loaded_meta(&case.worker.app_id)
            .expect("the reload recorded what it built against")
            .live_bindings,
        case.version.live_bindings,
        "and the isolate records it, so the next cycle does not reload again"
    );
    // The app is still serving: the reload replaced the isolate rather than
    // dropping it.
    assert_eq!(case.body().await, b"var-old|sec-old|var-old");
}

/// An app whose binding is WITHDRAWN reloads onto the set control now serves,
/// rather than keeping the edge the withdrawal retired.
///
/// The other direction of the same signal, and the one no SQLSTATE
/// distinguishes: `PostgreSQL` fences the retired role, so the app is not
/// exposed - it is BROKEN on that database until something rebuilds the
/// isolate, and this reconcile is the only thing that asks for one.
///
/// Both withdrawal shapes, because they take different paths: withdrawing one
/// of two re-reads control's set, and withdrawing the LAST one resolves to the
/// empty set with no read at all.
#[compio::test]
async fn an_app_whose_binding_is_withdrawn_reloads_onto_the_set_control_serves() {
    let mut case = deployed_with_database().await;
    let kept = case.resolved_binding();
    let withdrawn = ResolvedBinding {
        database: DatabaseId::mint(),
        binding: BindingId::mint(),
        capability: DatabaseCapability::ReadWrite,
    };
    // The converged starting state: the app holds TWO live bindings and its
    // isolate was built against both.
    binding_store()
        .supply(case.worker.app_id.as_str(), withdrawn.clone())
        .expect("the app's second binding");
    let before = live_bindings(&[kept.clone(), withdrawn.clone()]);
    case.version.live_bindings = before.clone();
    cache::set_loaded_meta(
        case.worker.app_id.clone(),
        cache::LoadedMeta {
            deploy_hash: case.version.deploy_hash.clone(),
            env_version: case.version.env_version,
            net_policy: case.version.net_policy.clone(),
            live_bindings: before.clone(),
        },
    );
    assert_eq!(
        case.installed_bindings(),
        before,
        "the premise: the store starts at the set the isolate was built from"
    );

    // ONE binding withdrawn: control serves the remainder and the worker
    // installs it whole.
    let bindings_route = route(endpoints::CONTROL_APP_BINDINGS, &case.worker.app_id);
    let control = ControlPlane::serving(
        1,
        vec![(
            bindings_route.clone(),
            binding_body(std::slice::from_ref(&kept)),
        )],
    );
    case.control_url = control.base_url.clone();
    case.version.live_bindings = live_bindings(std::slice::from_ref(&kept));

    case.reconcile().await;

    assert_eq!(control.served(), vec![bindings_route]);
    assert_eq!(
        case.installed_bindings(),
        case.version.live_bindings,
        "the withdrawn database is gone from the store, so no isolate built \
         from it composes the role the withdrawal retired"
    );
    assert!(
        !case.installed_bindings().contains_key(&withdrawn.database),
        "and it is that database specifically that left"
    );
    assert_eq!(
        cache::get_loaded_meta(&case.worker.app_id)
            .expect("the reload recorded what it built against")
            .live_bindings,
        case.version.live_bindings,
    );
    assert_eq!(case.body().await, b"var-old|sec-old|var-old");

    // The LAST binding withdrawn: the empty set is a statement, so the worker
    // unbinds the app rather than reading control for a set it was told is
    // empty. A control plane expecting NO request records one if it is made.
    let control = ControlPlane::serving(0, Vec::new());
    case.control_url = control.base_url.clone();
    case.version.live_bindings = std::collections::BTreeMap::new();

    case.reconcile().await;

    assert_eq!(
        control.served(),
        Vec::<String>::new(),
        "an app control serves no live binding for is unbound here, not read for"
    );
    assert!(
        case.installed_bindings().is_empty(),
        "the app holds no binding at all, the state it was in before any host \
         resolved one"
    );
    assert!(!binding_store()
        .is_bound(case.worker.app_id.as_str())
        .expect("read the store"));
    assert_eq!(case.body().await, b"var-old|sec-old|var-old");
}

/// An app whose binding is WITHDRAWN and RE-GRANTED for the same database at
/// the same capability, both inside one poll interval, reloads onto the fresh
/// edge.
///
/// The unbind DELETES the binding row (`databases::unbind_database`) and the
/// rebind MINTS a fresh id (`databases::bind_database`), so the role
/// `zs_bind_<binding>` the isolate composes is reaped and a differently named
/// one takes its place. Every other fact about the app holds still - same
/// deploy, same env, same limits, same policy, same database, same capability,
/// same number of bindings - so the live binding set is the only place this
/// can be seen, and it can only be seen there because each entry carries its
/// EDGE. Without the edge the feed entry is byte-identical across the move and
/// the isolate keeps composing a role `SET LOCAL ROLE` now refuses, leaving the
/// app refusing until its process restarts.
///
/// The case SHOWS it rather than asserting it: the database-to-capability
/// projection - everything a comparison ignoring the edge could see - is equal
/// on both sides, and the reload is asked for anyway. Its rejection
/// control is the reconcile after it, against a control plane expecting NO
/// request: with the sets agreeing, nothing re-resolves.
#[compio::test]
async fn an_app_rebound_to_the_same_database_reloads_onto_the_fresh_edge() {
    let mut case = deployed_with_database().await;
    let retired = case.resolved_binding();
    let regranted = ResolvedBinding {
        database: retired.database.clone(),
        binding: BindingId::mint(),
        capability: retired.capability,
    };
    assert_ne!(
        regranted.binding, retired.binding,
        "the premise: an unbind and a rebind are two edges"
    );
    let before = case.installed_bindings();
    assert_eq!(
        before.len(),
        1,
        "the premise: the store starts non-empty, so this is not passing over \
         an app that binds nothing"
    );
    let retired_binding = binding_store()
        .binding_for(case.worker.app_id.as_str(), "d", &retired.database)
        .expect("the app's isolate was built on an edge that composes a role");
    let retired_role = retired_binding
        .session_role()
        .expect("a resolved edge names a role")
        .to_owned();

    let served = vec![regranted.clone()];
    let bindings_route = route(endpoints::CONTROL_APP_BINDINGS, &case.worker.app_id);
    let control = ControlPlane::serving(1, vec![(bindings_route.clone(), binding_body(&served))]);
    case.control_url = control.base_url.clone();
    case.version.live_bindings = live_bindings(&served);
    assert_eq!(
        capabilities(&before),
        capabilities(&case.version.live_bindings),
        "the premise of this case: the database and its capability did not \
         move, which is everything a comparison ignoring the edge could see"
    );

    case.reconcile().await;

    assert_eq!(
        control.served(),
        vec![bindings_route],
        "the reload re-resolved the binding set at the route control declares"
    );
    assert_eq!(
        case.installed_bindings(),
        case.version.live_bindings,
        "the store holds the set control serves, so the isolate built above \
         narrows with the re-granted edge"
    );
    assert_eq!(
        case.resolved_edges()
            .get(&regranted.database)
            .map(|edge| edge.binding.clone()),
        Some(regranted.binding.clone()),
        "and it is the EDGE that moved: same database, new binding id"
    );
    let regranted_binding = binding_store()
        .binding_for(case.worker.app_id.as_str(), "d", &regranted.database)
        .expect("the re-granted edge composes a role");
    assert_ne!(
        regranted_binding
            .session_role()
            .expect("a resolved edge names a role"),
        retired_role,
        "which is the point: the role the isolate narrows to is derived from \
         the edge, so the rebind moved the role name the unbind reaped"
    );
    assert_eq!(
        cache::get_loaded_meta(&case.worker.app_id)
            .expect("the reload recorded what it built against")
            .live_bindings,
        case.version.live_bindings,
        "and the isolate records it, so the next cycle does not reload again"
    );
    // The app is still serving: the reload replaced the isolate rather than
    // dropping it.
    assert_eq!(case.body().await, b"var-old|sec-old|var-old");

    // THE REJECTION CONTROL, one variable changed: the sets now agree, and a
    // control plane expecting NO request records one if it is made. Without it
    // this case would pass over a reconcile that had started reloading every
    // app on every tick.
    let control = ControlPlane::serving(0, Vec::new());
    case.control_url = control.base_url.clone();
    case.reconcile().await;
    assert_eq!(
        control.served(),
        Vec::<String>::new(),
        "an app whose version feed entry did not move resolves nothing"
    );
    assert_eq!(case.installed_bindings(), case.version.live_bindings);
}

/// A reload whose binding set cannot be re-resolved keeps the previous isolate.
///
/// The same discipline as the bundle and env failures beside it: an isolate
/// built on a binding set control would not serve is worse than the one already
/// running, and the app comes back on the next cycle because nothing recorded
/// the reload as done.
#[compio::test]
async fn a_reload_that_cannot_re_resolve_the_binding_keeps_the_previous_app() {
    let mut case = deployed_with_database().await;
    let installed = case.resolved_binding();
    let before = case.installed_bindings();
    assert_eq!(before.len(), 1, "the premise: the store starts non-empty");
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
    let gained = ResolvedBinding {
        database: DatabaseId::mint(),
        binding: BindingId::mint(),
        capability: DatabaseCapability::ReadWrite,
    };
    let served = vec![installed, gained];
    case.version.live_bindings = live_bindings(&served);
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
        case.installed_bindings(),
        before,
        "a failed resolution installs nothing rather than unbinding the app"
    );

    // The control differing in ONE variable: with a control plane that serves
    // the set, the same reload completes.
    let bindings_route = route(endpoints::CONTROL_APP_BINDINGS, &case.worker.app_id);
    let control = ControlPlane::serving(1, vec![(bindings_route.clone(), binding_body(&served))]);
    case.control_url = control.base_url.clone();
    case.reconcile().await;
    assert_eq!(control.served(), vec![bindings_route]);
    assert_eq!(case.installed_bindings(), case.version.live_bindings);
    assert_eq!(case.body().await, b"replacement");
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

/// A deployed app on a worker that accounts for app credentials as `main`
/// composes it, with the app's project key supplied beside its binding and
/// environment.
async fn held_with_database(root: [u8; 32]) -> DeployedApp {
    let kernel = crate::cache::KernelConfig {
        workflows: None,
        db_service: Some(crate::cache::fixture::database_service(
            "postgresql://fixture:fixture@localhost/unused",
        )),
        kv_store: None,
        storage_backend: None,
        meter: Arc::new(zeroship_metering::Meter::new()),
        residency: None,
    };
    let case = DeployedApp::with_worker(crate::worker_fixture::Worker::holding(10, kernel)).await;
    crate::cache::project_keys()
        .expect("the kernel installed a database service")
        .supply(
            case.worker.app_id.as_str(),
            zeroship_core::ProjectId::mint().as_str(),
            root,
        )
        .expect("the app's project key");
    case
}

/// Which of the app's credentials this worker holds: key, bindings, env.
fn supplied(case: &DeployedApp) -> [bool; 3] {
    let app = case.worker.app_id.as_str();
    [
        crate::cache::project_keys()
            .expect("the kernel installed a database service")
            .is_bound(app)
            .expect("key store"),
        binding_store().is_bound(app).expect("binding store"),
        get_env(&case.worker.envs, &case.worker.app_id).is_some(),
    ]
}

/// A reload holds its app across the whole swap, so an eviction of the isolate
/// it is replacing withdraws nothing, and the replacement then serves without
/// Control being asked for anything again.
///
/// Every await of a swap hands this thread to other dispatches, and a dispatch
/// loading another app into a full cache evicts the least recently used
/// isolate - possibly the one being replaced, which was the app's only holder.
/// Control holds its answer to the swap's binding read while that eviction
/// happens, so it lands after the swap checked what was supplied and before it
/// built the replacement.
#[compio::test]
async fn a_reload_holds_its_app_across_the_swap() {
    let mut case = held_with_database([8; 32]).await;
    let registry = case.worker.residency.clone().expect("the case accounts for credentials");
    assert_eq!(
        registry.holders(&case.worker.app_id),
        1,
        "the premise: the isolate alone holds the app"
    );
    assert_eq!(supplied(&case), [true; 3]);

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
    let served = vec![
        case.resolved_binding(),
        ResolvedBinding {
            database: DatabaseId::mint(),
            binding: BindingId::mint(),
            capability: DatabaseCapability::ReadWrite,
        },
    ];
    case.version.live_bindings = live_bindings(&served);
    let bindings_route = route(endpoints::CONTROL_APP_BINDINGS, &case.worker.app_id);
    let (control, mut gate) = ControlPlane::gated(
        1,
        vec![(bindings_route.clone(), binding_body(&served))],
        bindings_route.clone(),
    );
    case.control_url = control.base_url.clone();

    {
        let mut reconciling = Box::pin(case.reconcile());
        match futures::future::select(Box::pin(gate.arrived()), reconciling.as_mut()).await {
            futures::future::Either::Left(((), _)) => {}
            futures::future::Either::Right(((), _)) => {
                panic!("the reload finished before Control answered its binding read")
            }
        }
        // Another dispatch's eviction of the isolate being replaced.
        cache::evict_app(&case.worker.app_id);
        assert!(cache::get_runtime(&case.worker.app_id).is_none());
        gate.release();
        reconciling.await;
    }

    assert_eq!(control.served(), vec![bindings_route]);
    assert_eq!(
        supplied(&case),
        [true; 3],
        "the swap's own hold kept the key, the bindings and the environment"
    );
    assert_eq!(case.installed_bindings(), case.version.live_bindings);
    assert_eq!(
        case.body().await,
        b"replacement",
        "the replacement serves, and Control is not asked for anything again"
    );
    assert_eq!(registry.holders(&case.worker.app_id), 1);
}

/// An environment refresh holds the app while it supplies, and skips an app
/// nothing holds.
///
/// The refresh walks a snapshot of the environments present; an app whose last
/// holder dropped after that snapshot had its material withdrawn, and
/// refreshing it would supply a key and an environment no holder will ever
/// withdraw. The fixture's app is exactly that: its environment is present and
/// nothing holds it. The control is the same refresh with the app held, which
/// does fetch.
#[compio::test]
async fn an_environment_refresh_skips_an_app_nothing_holds() {
    let kernel = crate::cache::KernelConfig {
        workflows: None,
        db_service: Some(crate::cache::fixture::database_service(
            "postgresql://fixture:fixture@localhost/unused",
        )),
        kv_store: None,
        storage_backend: None,
        meter: Arc::new(zeroship_metering::Meter::new()),
        residency: None,
    };
    let worker = crate::worker_fixture::Worker::holding(10, kernel);
    let app = worker.app_id.clone();
    let registry = worker.residency.clone().expect("the case accounts for credentials");
    let keys = crate::cache::project_keys().expect("the kernel installed a database service");
    assert_eq!(registry.holders(&app), 0, "the premise: nothing holds the app");
    assert_eq!(cached_env_version(&worker.envs, &app), Some(0));
    let env = r#"{"vars":{"API_TOKEN":"rotated"},"secrets":{},"expose":[]}"#;
    let routes = vec![
        (
            route(endpoints::CONTROL_APP_DATA_KEY, &app),
            serde_json::to_string(&zeroship_core::project_data_key::ProjectDataKey::new(
                zeroship_core::ProjectId::mint(),
                [9; 32],
            ))
            .expect("project key"),
        ),
        (route(endpoints::CONTROL_APP_ENV, &app), env.to_owned()),
    ];
    let versions = VersionMap::from([(
        app.clone(),
        AppVersionInfo {
            deploy_hash: None,
            plan_id: "starter".into(),
            runtime: crate::cache::TEST_LIMITS,
            env_version: 2,
            manifest: None,
            net_policy: AppNetPolicy::default(),
            live_bindings: binding_store().live_bindings_for(app.as_str()),
        },
    )]);
    let config = |control_url: &str| crate::WorkerConfig {
        service_auth: worker.config.service_auth.clone(),
        control_url: control_url.to_owned(),
        control_key: String::new(),
        kv_store: None,
        storage_backend: None,
        max_isolates: worker.config.max_isolates,
        poll_interval_secs: worker.config.poll_interval_secs,
        shutdown_timeout_secs: worker.config.shutdown_timeout_secs,
        blob_store: worker.config.blob_store.clone(),
    };

    // Served if asked, so a refresh that went ahead would leave its mark.
    let unheld = ControlPlane::serving(2, routes.clone());
    reconcile_once(&config(&unheld.base_url), &versions, &worker.envs)
        .await
        .expect("reconcile");
    assert_eq!(cached_env_version(&worker.envs, &app), Some(0), "nothing refreshed");
    assert!(!keys.is_bound(app.as_str()).unwrap(), "and no key was supplied");

    // THE CONTROL: the same refresh with the app held.
    let _held = registry.reside(app.clone());
    let control = ControlPlane::serving(2, routes);
    reconcile_once(&config(&control.base_url), &versions, &worker.envs)
        .await
        .expect("reconcile");
    assert_eq!(
        control.served(),
        vec![
            route(endpoints::CONTROL_APP_DATA_KEY, &app),
            route(endpoints::CONTROL_APP_ENV, &app),
        ]
    );
    assert_eq!(cached_env_version(&worker.envs, &app), Some(2));
    assert!(keys.is_bound(app.as_str()).unwrap());
}
