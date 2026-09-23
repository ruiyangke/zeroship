use super::*;
use std::collections::BTreeMap;
use zeroship_core::database_role::DatabaseCapability;
use zeroship_core::DatabaseId;

fn version_info(
    deploy_hash: Option<&str>,
    env_version: i64,
    runtime: AppRuntimeLimits,
) -> AppVersionInfo {
    AppVersionInfo {
        deploy_hash: deploy_hash.map(str::to_string),
        plan_id: "starter".to_string(),
        runtime,
        env_version,
        manifest: None,
        net_policy: AppNetPolicy::default(),
        live_bindings: BTreeMap::new(),
    }
}

fn loaded_meta(deploy_hash: Option<&str>, env_version: i64) -> cache::LoadedMeta {
    cache::LoadedMeta {
        deploy_hash: deploy_hash.map(str::to_string),
        env_version,
        net_policy: AppNetPolicy::default(),
        live_bindings: BTreeMap::new(),
    }
}

/// One app's live binding set, spelled as the pairs a case cares about.
fn bindings(
    entries: &[(&DatabaseId, DatabaseCapability)],
) -> BTreeMap<DatabaseId, DatabaseCapability> {
    entries
        .iter()
        .map(|(database, capability)| ((*database).clone(), *capability))
        .collect()
}

/// The scalar a count over the set would summarise it as. Named so a case can
/// SHOW that the number it refutes did not move, rather than assert in prose
/// that it would not have.
fn size(set: &BTreeMap<DatabaseId, DatabaseCapability>) -> usize {
    set.len()
}

/// An app that has GAINED a binding must be replaced: its isolate was built
/// with no handle for that database, and the handle is materialized once, while
/// the isolate builds.
///
/// Its rejection control is the same app at the SAME set, which must not
/// reload. Without it this would pass over a `needs_reload` that had started
/// answering true for everything.
#[test]
fn needs_reload_true_when_a_database_is_bound_under_a_running_isolate() {
    let main = DatabaseId::mint();
    let analytics = DatabaseId::mint();
    let mut info = version_info(Some("h1"), 7, AppRuntimeLimits::default());
    info.live_bindings = bindings(&[
        (&main, DatabaseCapability::ReadWrite),
        (&analytics, DatabaseCapability::ReadOnly),
    ]);
    let mut loaded = loaded_meta(Some("h1"), 7);
    loaded.live_bindings = bindings(&[(&main, DatabaseCapability::ReadWrite)]);
    assert!(
        needs_reload(Some(&loaded), Some(matching_limits(&info.runtime)), &info),
        "a database the app has started binding needs an isolate built with a \
         handle for it; nothing else about the app moved"
    );

    loaded.live_bindings = info.live_bindings.clone();
    assert!(
        !needs_reload(Some(&loaded), Some(matching_limits(&info.runtime)), &info),
        "an app whose binding set did not move must not reload: reload churn \
         drops module state and in-flight work for nothing"
    );
}

/// An app whose binding was WITHDRAWN must be replaced onto the set control now
/// serves, rather than keep composing the role the withdrawal retired.
///
/// The withdrawn direction is not symmetric with the gained one: `PostgreSQL`
/// fences the retired role at `SET LOCAL ROLE`, so the app is not exposed - it
/// is BROKEN on that database until something rebuilds the isolate, and this
/// comparison is the only thing that asks for one.
#[test]
fn needs_reload_true_when_a_binding_is_withdrawn_under_a_running_isolate() {
    let main = DatabaseId::mint();
    let analytics = DatabaseId::mint();
    let mut info = version_info(Some("h1"), 7, AppRuntimeLimits::default());
    info.live_bindings = bindings(&[(&main, DatabaseCapability::ReadWrite)]);
    let mut loaded = loaded_meta(Some("h1"), 7);
    loaded.live_bindings = bindings(&[
        (&main, DatabaseCapability::ReadWrite),
        (&analytics, DatabaseCapability::ReadOnly),
    ]);
    assert!(
        needs_reload(Some(&loaded), Some(matching_limits(&info.runtime)), &info),
        "control serves no live binding for the second database any more, and \
         an isolate holding one composes a role the withdrawal retired"
    );

    // And the LAST binding withdrawn, which leaves the empty set: the app has
    // no `env.db` at all, and the empty set is a statement rather than an
    // absence.
    loaded.live_bindings = bindings(&[(&main, DatabaseCapability::ReadWrite)]);
    info.live_bindings = BTreeMap::new();
    assert!(
        needs_reload(Some(&loaded), Some(matching_limits(&info.runtime)), &info),
        "an app whose last binding was withdrawn must be rebuilt without one"
    );

    // The rejection control for both arms: equal sets do not reload.
    loaded.live_bindings = BTreeMap::new();
    assert!(!needs_reload(
        Some(&loaded),
        Some(matching_limits(&info.runtime)),
        &info
    ));
}

/// A CAPABILITY edit under a set whose size never moved must reload.
///
/// This is the case a count over the set loses. The test SHOWS it: the two sets
/// hold the same number of databases and the same databases, so a `needs_reload`
/// comparing a count would answer false here while the isolate kept narrowing
/// to the read-write role of a binding control has since made read-only.
#[test]
fn needs_reload_true_when_a_capability_changes_under_an_unchanged_set_size() {
    let main = DatabaseId::mint();
    let mut info = version_info(Some("h1"), 7, AppRuntimeLimits::default());
    info.live_bindings = bindings(&[(&main, DatabaseCapability::ReadOnly)]);
    let mut loaded = loaded_meta(Some("h1"), 7);
    loaded.live_bindings = bindings(&[(&main, DatabaseCapability::ReadWrite)]);

    assert_eq!(
        size(&loaded.live_bindings),
        size(&info.live_bindings),
        "the premise of this case: the number of live bindings did not move"
    );
    assert_eq!(
        loaded.live_bindings.keys().collect::<Vec<_>>(),
        info.live_bindings.keys().collect::<Vec<_>>(),
        "and neither did which databases are bound"
    );
    assert!(
        needs_reload(Some(&loaded), Some(matching_limits(&info.runtime)), &info),
        "the capability moved, so the isolate holds a handle control no longer \
         serves at that capability"
    );

    // The rejection control, one variable changed back.
    loaded.live_bindings = bindings(&[(&main, DatabaseCapability::ReadOnly)]);
    assert!(!needs_reload(
        Some(&loaded),
        Some(matching_limits(&info.runtime)),
        &info
    ));
}

fn matching_limits(runtime: &AppRuntimeLimits) -> RuntimeLimits {
    cache::runtime_limits_from_app(runtime)
}

#[test]
fn needs_reload_false_when_state_matches() {
    let info = version_info(Some("h1"), 7, AppRuntimeLimits::default());
    let loaded = loaded_meta(Some("h1"), 7);
    assert!(
        !needs_reload(Some(&loaded), Some(matching_limits(&info.runtime)), &info),
        "no hash / limits / env change must NOT reload (reload churn would \
         drop module state + in-flight work for nothing)"
    );
}

#[test]
fn needs_reload_true_when_only_env_version_bumps() {
    let info = version_info(Some("h1"), 2, AppRuntimeLimits::default());
    let loaded = loaded_meta(Some("h1"), 1);
    assert!(
        needs_reload(Some(&loaded), Some(matching_limits(&info.runtime)), &info),
        "SEC-7: env-only version bump (hash + limits unchanged) must \
         reload the isolate so secret rotation actually applies"
    );
}

#[test]
fn poll_interval_of_zero_is_rejected() {
    assert!(
        super::validate_poll_interval_secs(0).is_err(),
        "zero divides the reconcile jitter and would panic every reconcile \
         task while the server kept serving"
    );
    assert_eq!(super::validate_poll_interval_secs(1), Ok(1));
    assert_eq!(super::validate_poll_interval_secs(60), Ok(60));
}

#[test]
fn needs_reload_true_when_the_deploy_goes_away() {
    let info = version_info(None, 7, AppRuntimeLimits::default());
    let loaded = loaded_meta(Some("h1"), 7);
    assert!(
        needs_reload(Some(&loaded), Some(matching_limits(&info.runtime)), &info),
        "an app that lost its live deploy must reload rather than keep \
         serving the code it had cached"
    );
}

#[test]
fn needs_reload_true_when_deploy_hash_changes() {
    let info = version_info(Some("h2"), 7, AppRuntimeLimits::default());
    let loaded = loaded_meta(Some("h1"), 7);
    assert!(needs_reload(
        Some(&loaded),
        Some(matching_limits(&info.runtime)),
        &info
    ));
}

#[test]
fn needs_reload_true_when_limits_change() {
    let info = version_info(
        Some("h1"),
        7,
        AppRuntimeLimits {
            cpu_limit_ms: Some(123),
            ..AppRuntimeLimits::default()
        },
    );
    let loaded = loaded_meta(Some("h1"), 7);
    assert!(needs_reload(
        Some(&loaded),
        Some(matching_limits(&AppRuntimeLimits::default())),
        &info
    ));
}

#[test]
fn needs_reload_true_when_net_policy_changes() {
    let info = version_info(Some("h1"), 7, AppRuntimeLimits::default());
    let loaded = cache::LoadedMeta {
        deploy_hash: Some("h1".to_string()),
        env_version: 7,
        net_policy: AppNetPolicy {
            egress: vec![NetEgressEntry {
                verdict: Verdict::Accept,
                destination: "db.example.com".to_string(),
                port: 5432,
            }],
            max_sockets: 4,
            egress_ceiling_bytes: 1024 * 1024,
        },
        live_bindings: BTreeMap::new(),
    };
    assert!(
        needs_reload(Some(&loaded), Some(matching_limits(&info.runtime)), &info),
        "revoking the last net grant changes AppVersionInfo.net_policy to \
         default-deny and must rebuild the isolate on the next reconcile tick"
    );
}

#[test]
fn needs_reload_true_when_isolate_meta_missing() {
    let info = version_info(Some("h1"), 0, AppRuntimeLimits::default());
    assert!(needs_reload(
        None,
        Some(matching_limits(&info.runtime)),
        &info
    ));
}
