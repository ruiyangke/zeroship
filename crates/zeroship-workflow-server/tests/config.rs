use clap::Parser;
use std::{os::unix::fs::PermissionsExt, process::Command};
use zeroship_core::config::GeneratedConfig;
use zeroship_workflow_server::{
    config::{WorkflowSettings, WorkflowSettingsSources},
    server::ServerOptions,
};

#[test]
fn config_check_validates_toml_and_flags_without_opening_dependencies() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("zeroship.toml");
    std::fs::write(
        &config,
        toml::to_string(&serde_json::json!({"workflow":{
            "database_url":"postgres://unused:private-workflow-password@127.0.0.1:1/unreachable",
            "service_peers_file":dir.path().join("unread-peers"),
            "service_key_file":dir.path().join("unread-workflow-key"),
            "control_url":"https://control.example.test",
            "batch_limit":3,
            "driver_interval_ms":250,
            "driver_lane_timeout_ms":1500,
            "closing_idle_ms":5000,
            "closing_timeout_ms":4000,
            "closing_backoff_ms":300,
            "closing_backoff_max_ms":900,
            "policy_cache_entries":7,
        }}))
        .unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_zeroship-workflow-server"))
        .env_clear()
        .args([
            "--check-config",
            "--check-config-format",
            "json",
            "--config",
        ])
        .arg(&config)
        .args(["--batch-limit", "2"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let output = String::from_utf8(result.stdout).unwrap();
    assert!(!output.contains("private-workflow-password"));
    assert!(output.contains("database_configured"));

    let overlay: toml::Value = toml::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    let settings = WorkflowSettings::resolve_config(
        WorkflowSettingsSources::try_parse_from([
            "zeroship-workflow-server",
            "--check-config",
            "--batch-limit",
            "2",
        ])
        .unwrap(),
        Some(&overlay),
    )
    .unwrap();
    assert_eq!(*settings.batch_limit.get(), 2);
    let options = ServerOptions::resolve(&settings).unwrap();
    assert_eq!(options.policy_cache_entries.get(), 7);
    assert_eq!(options.driver.page_limit, 2);
    assert_eq!(
        options.driver_interval,
        std::time::Duration::from_millis(250)
    );
    assert_eq!(
        options.driver.lane_timeout,
        std::time::Duration::from_millis(1500)
    );
    let recovery = options.driver.recovery;
    assert_eq!(recovery.idle_after, std::time::Duration::from_millis(5000));
    assert_eq!(
        recovery.closing_timeout,
        std::time::Duration::from_millis(4000)
    );
    assert_eq!(
        recovery.closing_backoff,
        std::time::Duration::from_millis(300)
    );
    assert_eq!(
        recovery.closing_backoff_max,
        std::time::Duration::from_millis(900)
    );

    let missing = Command::new(env!("CARGO_BIN_EXE_zeroship-workflow-server"))
        .env_clear()
        .args(["--no-config", "--check-config"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(WorkflowSettingsSources::try_parse_from([
        "zeroship-workflow-server",
        "--database-url",
        "secret"
    ])
    .is_err());
    let invalid = Command::new(env!("CARGO_BIN_EXE_zeroship-workflow-server"))
        .env_clear()
        .args(["--check-config", "--config"])
        .arg(&config)
        .args(["--assignment-ttl-ms", "0"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    for flag in [
        "--driver-interval-ms",
        "--driver-lane-timeout-ms",
        "--policy-cache-entries",
    ] {
        let invalid = Command::new(env!("CARGO_BIN_EXE_zeroship-workflow-server"))
            .env_clear()
            .args(["--check-config", "--config"])
            .arg(&config)
            .args([flag, "0"])
            .output()
            .unwrap();
        assert!(!invalid.status.success(), "accepted {flag}=0");
    }
    for flag in ["--payload-url", "--max-running", "--lease-ms"] {
        assert!(WorkflowSettingsSources::try_parse_from([
            "zeroship-workflow-server",
            flag,
            "unused"
        ])
        .is_err());
    }
}

#[test]
fn hold_release_grace_outlasts_the_queue_transaction_budget() {
    let resolve = |command_timeout_ms: u64| {
        let overlay = toml::from_str(
            &toml::to_string(&serde_json::json!({"workflow":{
                "database_url":"postgres://unused@127.0.0.1:1/unreachable",
                "service_peers_file":"unread-peers", "service_key_file":"unread-key",
                "control_url":"https://control.example.test",
                "database_command_timeout_ms":command_timeout_ms,
            }}))
            .unwrap(),
        )
        .unwrap();
        let settings = WorkflowSettings::resolve_config(
            WorkflowSettingsSources::try_parse_from(["zeroship-workflow-server", "--no-config"])
                .unwrap(),
            Some(&overlay),
        )
        .unwrap();
        ServerOptions::resolve(&settings).unwrap()
    };
    let short = resolve(1_000);
    let long = resolve(3_600_000);
    // The command timeout bounds each queue transaction, including the commit
    // of a dependency whose hold was confirmed outside it.
    for options in [&short, &long] {
        assert!(options.driver.hold_grace > options.coordinator.command_timeout);
    }
    assert!(long.driver.hold_grace > short.driver.hold_grace);
}

#[test]
fn retention_configuration_requires_a_signer_and_unambiguous_control_origin() {
    let valid = serde_json::json!({"workflow":{
        "database_url":"postgres://unused@127.0.0.1:1/unreachable",
        "service_peers_file":"unread-peers", "service_key_file":"unread-key",
        "control_url":"https://control.example.test",
    }});
    let resolve = |input: &serde_json::Value| {
        let overlay = toml::from_str(&toml::to_string(input).unwrap()).unwrap();
        let settings = WorkflowSettings::resolve_config(
            WorkflowSettingsSources::try_parse_from(["zeroship-workflow-server", "--no-config"])
                .unwrap(),
            Some(&overlay),
        )
        .unwrap();
        ServerOptions::resolve(&settings)
    };
    resolve(&valid).unwrap();
    for field in ["control_url", "service_key_file"] {
        let mut missing = valid.clone();
        missing["workflow"].as_object_mut().unwrap().remove(field);
        assert!(resolve(&missing).is_err(), "missing {field}");
    }
    for url in [
        "http://control.example.test",
        "https://user:secret@control.example.test",
        "https://control.example.test/path",
        "https://control.example.test?query",
        "https://control.example.test#fragment",
        "not a URL",
    ] {
        let mut invalid = valid.clone();
        invalid["workflow"]["control_url"] = url.into();
        assert!(resolve(&invalid).is_err(), "accepted {url}");
    }
    for url in ["http://127.0.0.1:9090", "http://[::1]:9090"] {
        let mut loopback = valid.clone();
        loopback["workflow"]["control_url"] = url.into();
        resolve(&loopback).unwrap();
    }
}
