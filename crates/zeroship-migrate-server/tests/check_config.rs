//! Process-level configuration checks for the migration service.
//!
//! These launch Cargo's freshly built binary with a private environment. That
//! exercises the shipped clap carrier, bootstrap resolver, credential posture,
//! report renderer, and early return without sharing process environment or
//! filesystem fixtures with another test.

use std::path::PathBuf;
use std::process::{Command, Output};

const STRONG_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "zeroship-migrate-check-config-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&path).expect("create check-config scratch directory");
        Self(path)
    }

    fn child(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zeroship-migrate-server"))
        .env_clear()
        .env("ZEROSHIP_CONTROL_KEY", STRONG_HEX)
        .env("ZEROSHIP_MIGRATE_SERVER_POLICY_SEAL_KEY", STRONG_HEX)
        .env(
            "ZEROSHIP_MIGRATE_SERVER_DATABASE_URL",
            "postgresql://unused:unused@127.0.0.1:1/unused",
        )
        .env(
            "ZEROSHIP_MIGRATE_SERVER_PROVISION_DATABASE_URL",
            "postgresql://unused:unused@127.0.0.1:1/unused",
        )
        .args(args)
        .output()
        .expect("spawn zeroship-migrate-server")
}

fn report(stdout: &[u8]) -> serde_json::Map<String, serde_json::Value> {
    let text = String::from_utf8_lossy(stdout);
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|value| value.as_object().cloned())
        .unwrap_or_else(|| panic!("no JSON check-config report in stdout:\n{text}"))
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "check-config failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn check_config_reports_json_without_connecting_or_creating_runtime_state() {
    let scratch = Scratch::new();
    let runtime_dir = scratch.child("runtime-must-remain-absent");
    let runtime_dir_arg = runtime_dir.to_str().expect("UTF-8 scratch path");

    let output = run(&[
        "--check-config",
        "--check-config-format",
        "json",
        "--no-config",
        "--tmp-dir",
        runtime_dir_arg,
    ]);

    assert_success(&output);
    assert!(
        !runtime_dir.exists(),
        "check-config created runtime state at {}",
        runtime_dir.display()
    );

    let report = report(&output.stdout);
    assert_eq!(
        report.get("tmp_dir"),
        Some(&serde_json::Value::String(runtime_dir_arg.to_owned()))
    );
    assert_eq!(
        report
            .get("config_source")
            .and_then(serde_json::Value::as_str),
        Some("(none)")
    );
    assert_eq!(
        report
            .get("db_configured")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        report
            .get("provision_db_configured")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        report
            .get("policy_seal_key_configured")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
}

#[test]
fn check_config_rejects_an_unknown_report_format() {
    let output = run(&[
        "--check-config",
        "--check-config-format",
        "yaml",
        "--no-config",
    ]);

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid value 'yaml'") && stderr.contains("--check-config-format"),
        "unexpected clap diagnostic:\n{stderr}"
    );
}

#[test]
fn check_config_does_not_open_secret_path_flags() {
    let scratch = Scratch::new();
    let seal = scratch.child("absent-policy-seal");
    let control = scratch.child("absent-control-key");

    let output = Command::new(env!("CARGO_BIN_EXE_zeroship-migrate-server"))
        .env_clear()
        .args([
            "--check-config",
            "--check-config-format",
            "json",
            "--no-config",
            "--policy-seal-key-file",
        ])
        .arg(&seal)
        .arg("--control-key-file")
        .arg(&control)
        .output()
        .expect("spawn zeroship-migrate-server");

    assert_success(&output);
    assert!(!seal.exists());
    assert!(!control.exists());
    let report = report(&output.stdout);
    assert_eq!(
        report
            .get("policy_seal_key_configured")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        report
            .get("control_key_configured")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
}
