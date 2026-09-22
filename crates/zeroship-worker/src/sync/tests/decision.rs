use super::*;

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
    }
}

fn loaded_meta(deploy_hash: Option<&str>, env_version: i64) -> cache::LoadedMeta {
    cache::LoadedMeta {
        deploy_hash: deploy_hash.map(str::to_string),
        env_version,
        net_policy: AppNetPolicy::default(),
    }
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
