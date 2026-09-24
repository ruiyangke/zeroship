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
fn capacity_bounds_and_pacing_come_from_configuration_and_reject_an_empty_range() {
    let valid = serde_json::json!({"workflow":{
        "database_url":"postgres://unused@127.0.0.1:1/unreachable",
        "service_peers_file":"unread-peers", "service_key_file":"unread-key",
        "control_url":"https://control.example.test",
        "capacity_min_slots":2,
        "capacity_max_slots":9,
        "capacity_hold_down_ms":7000,
        "capacity_request_timeout_ms":1500,
        "capacity_retry_interval_ms":2500,
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
    let capacity = resolve(&valid).unwrap().driver.capacity;
    assert_eq!((capacity.min_slots, capacity.max_slots), (2, 9));
    assert_eq!(
        capacity.idle_hold_down,
        std::time::Duration::from_millis(7000)
    );
    assert_eq!(
        capacity.request_timeout,
        std::time::Duration::from_millis(1500)
    );
    assert_eq!(
        capacity.retry_interval,
        std::time::Duration::from_millis(2500)
    );
    // A floor above the ceiling names no reachable target, and a zero pacing
    // value would request without pause.
    for (field, value) in [
        ("capacity_min_slots", serde_json::json!(10)),
        ("capacity_max_slots", serde_json::json!(0)),
        ("capacity_min_slots", serde_json::json!(-1)),
        ("capacity_hold_down_ms", serde_json::json!(0)),
        ("capacity_request_timeout_ms", serde_json::json!(0)),
        ("capacity_retry_interval_ms", serde_json::json!(0)),
    ] {
        let mut invalid = valid.clone();
        invalid["workflow"][field] = value.clone();
        assert!(resolve(&invalid).is_err(), "accepted {field}={value}");
    }
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

    // The named-peer arm, through the whole settings surface: a root-level
    // `plaintext_peers` list admits the ONE origin it names. `http://control
    // .example.test` is refused above under the same overlay without it, so the
    // difference is the list rather than the origin.
    let mut named = valid.clone();
    named["workflow"]["control_url"] = "http://control.example.test".into();
    named["plaintext_peers"] = serde_json::json!(["http://control.example.test"]);
    let options = resolve(&named).expect("a named plaintext peer is a usable control origin");
    assert_eq!(
        options.plaintext_peers.joined(),
        "http://control.example.test"
    );

    // One variable from that: a list naming a DIFFERENT origin leaves the same
    // control_url refused.
    let mut elsewhere = named.clone();
    elsewhere["plaintext_peers"] = serde_json::json!(["http://migrate.example.test:9091"]);
    assert!(resolve(&elsewhere).is_err());

    // An entry that is not one exact http origin is refused while RESOLVING
    // THE SETTINGS, before any origin is judged against it, rather than
    // silently admitting nothing. That is a step earlier than `resolve` above
    // reaches, so it is driven through `resolve_config` directly.
    let settings = |input: &serde_json::Value| {
        let overlay: toml::Value = toml::from_str(&toml::to_string(input).unwrap()).unwrap();
        WorkflowSettings::resolve_config(
            WorkflowSettingsSources::try_parse_from(["zeroship-workflow-server", "--no-config"])
                .unwrap(),
            Some(&overlay),
        )
        .map(|resolved| resolved.plaintext_peers.get().clone())
    };
    // The control: the well-formed list above resolves, so the refusals below
    // are about the entry rather than about the key being unreadable.
    assert_eq!(settings(&named).expect("a well-formed list resolves").len(), 1);
    for entry in [
        // https authorizes nothing here; accepting it would let an operator
        // believe a peer was listed when the list is about plaintext alone.
        "https://control.example.test",
        "control.example.test",
        "*",
        "http://*.example.test",
        "http://user:secret@control.example.test",
        "http://control.example.test/prefix",
    ] {
        let mut malformed = named.clone();
        malformed["plaintext_peers"] = serde_json::json!([entry]);
        assert!(settings(&malformed).is_err(), "accepted peer {entry}");
    }
}
