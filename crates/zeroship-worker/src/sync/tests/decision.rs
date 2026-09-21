use super::*;
use std::collections::BTreeMap;
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
        binding_epochs: BTreeMap::new(),
    }
}

fn loaded_meta(deploy_hash: Option<&str>, env_version: i64) -> cache::LoadedMeta {
    cache::LoadedMeta {
        deploy_hash: deploy_hash.map(str::to_string),
        env_version,
        net_policy: AppNetPolicy::default(),
        binding_epochs: BTreeMap::new(),
    }
}

/// One app's per-database epochs, spelled as the pairs a case cares about.
fn epochs(entries: &[(&DatabaseId, u32)]) -> BTreeMap<DatabaseId, u32> {
    entries
        .iter()
        .map(|(database, epoch)| ((*database).clone(), *epoch))
        .collect()
}

/// The two scalars a single number could summarise this map as. Named so a
/// case can SHOW that the scalar it refutes did not move, rather than assert
/// in prose that it would not have.
fn highest(map: &BTreeMap<DatabaseId, u32>) -> Option<u32> {
    map.values().copied().max()
}

fn total(map: &BTreeMap<DatabaseId, u32>) -> u32 {
    map.values().copied().sum()
}

/// An isolate whose bound database rotated must be replaced: its sessions
/// narrow to a role name carrying the epoch it was built against, and the
/// apply after next drops that role.
///
/// Its rejection control is the same app at the SAME epoch, which must not
/// reload - otherwise this would pass over a `needs_reload` that had started
/// answering true for everything.
#[test]
fn needs_reload_true_when_a_bound_databases_epoch_advances() {
    let database = DatabaseId::mint();
    let mut info = version_info(Some("h1"), 7, AppRuntimeLimits::default());
    info.binding_epochs = epochs(&[(&database, 5)]);
    let mut loaded = loaded_meta(Some("h1"), 7);
    loaded.binding_epochs = epochs(&[(&database, 4)]);
    assert!(
        needs_reload(Some(&loaded), Some(matching_limits(&info.runtime)), &info),
        "an apply that advanced the schema epoch retires the role this \
         isolate's sessions narrow to, so the isolate must be replaced"
    );

    loaded.binding_epochs = epochs(&[(&database, 5)]);
    assert!(
        !needs_reload(Some(&loaded), Some(matching_limits(&info.runtime)), &info),
        "an app whose epoch did not move must not reload: reload churn drops \
         module state and in-flight work for nothing"
    );
}

/// An app binds MANY databases, and the second one advancing under a
/// higher-epoch first one must reload.
///
/// This is the case a single scalar loses. The test SHOWS it: the highest
/// epoch across the set is identical on both sides, so a `needs_reload`
/// comparing a maximum would answer false here while the analytics binding's
/// every session was refused at `SET LOCAL ROLE`.
#[test]
fn needs_reload_true_when_a_second_database_advances_under_a_higher_first_one() {
    let main = DatabaseId::mint();
    let analytics = DatabaseId::mint();
    let mut info = version_info(Some("h1"), 7, AppRuntimeLimits::default());
    info.binding_epochs = epochs(&[(&main, 9), (&analytics, 3)]);
    let mut loaded = loaded_meta(Some("h1"), 7);
    loaded.binding_epochs = epochs(&[(&main, 9), (&analytics, 2)]);

    assert_eq!(
        highest(&loaded.binding_epochs),
        highest(&info.binding_epochs),
        "the premise of this case: the highest epoch in the set did not move"
    );
    assert!(
        needs_reload(Some(&loaded), Some(matching_limits(&info.runtime)), &info),
        "the second database advanced, so the isolate holds a retired role for \
         it however high the first database's epoch is"
    );

    // The rejection control, one variable changed back: the same two-database
    // app with neither epoch moved must not reload.
    loaded.binding_epochs = epochs(&[(&main, 9), (&analytics, 3)]);
    assert!(!needs_reload(
        Some(&loaded),
        Some(matching_limits(&info.runtime)),
        &info
    ));
}

/// A binding WITHDRAWN while another advances must reload, and this is the
/// case a total over the set loses: the sum is identical on both sides.
///
/// A withdrawn binding is the condition no SQLSTATE distinguishes - the
/// classifier's input is the binding it was handed, so holding one is not
/// evidence that it is live. Re-resolution is what distinguishes it, and this
/// comparison is what asks for one.
#[test]
fn needs_reload_true_when_a_withdrawn_binding_leaves_the_total_unchanged() {
    let main = DatabaseId::mint();
    let analytics = DatabaseId::mint();
    let mut info = version_info(Some("h1"), 7, AppRuntimeLimits::default());
    info.binding_epochs = epochs(&[(&main, 11)]);
    let mut loaded = loaded_meta(Some("h1"), 7);
    loaded.binding_epochs = epochs(&[(&main, 9), (&analytics, 2)]);

    assert_eq!(
        total(&loaded.binding_epochs),
        total(&info.binding_epochs),
        "the premise of this case: the epochs sum to the same number"
    );
    assert!(
        needs_reload(Some(&loaded), Some(matching_limits(&info.runtime)), &info),
        "control serves no live binding for the second database any more, and \
         an isolate holding one composes a role a revocation retired"
    );

    // And the other direction: an app that has STARTED binding a database
    // reloads too, because its isolate was built with no handle for it.
    loaded.binding_epochs = epochs(&[(&main, 11)]);
    info.binding_epochs = epochs(&[(&main, 11), (&analytics, 0)]);
    assert!(
        needs_reload(Some(&loaded), Some(matching_limits(&info.runtime)), &info),
        "a database the app has started binding needs an isolate built with a \
         handle for it"
    );

    // The rejection control for both arms: equal sets do not reload.
    loaded.binding_epochs = epochs(&[(&main, 11), (&analytics, 0)]);
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
        binding_epochs: BTreeMap::new(),
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
