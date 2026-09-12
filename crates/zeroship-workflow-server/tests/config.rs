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
            "batch_limit":3,
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
    ServerOptions::resolve(&settings).unwrap();

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
        "--payload-url",
        "--service-key-file",
        "--max-running",
        "--lease-ms",
    ] {
        assert!(WorkflowSettingsSources::try_parse_from([
            "zeroship-workflow-server",
            flag,
            "unused"
        ])
        .is_err());
    }
}
