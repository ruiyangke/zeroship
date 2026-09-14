//! Process-level configuration checks for the worker binary.
//!
//! The worker deliberately has no shared overlay. These tests launch Cargo's
//! freshly built binary so the assertion covers the shipped argument parser,
//! resolver, report, and early return without mutating the test process's
//! environment.

use std::process::{Command, Output};

const STRONG_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zeroship-worker"))
        .env_clear()
        .env("ZEROSHIP_CONTROL_KEY", STRONG_HEX)
        .args(args)
        .output()
        .expect("spawn zeroship-worker")
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
fn check_config_reports_json_without_contacting_control_or_creating_blob_state() {
    let scratch = tempfile::tempdir().expect("create check-config scratch directory");
    let blob_store = scratch.path().join("blob-store-must-remain-absent");
    let blob_store_arg = blob_store.to_str().expect("UTF-8 scratch path");
    let control_url = "http://127.0.0.1:1";

    let output = run(&[
        "--check-config",
        "--check-config-format",
        "json",
        "--control-url",
        control_url,
        "--blob-store",
        blob_store_arg,
    ]);

    assert_success(&output);
    assert!(
        !blob_store.exists(),
        "check-config created blob state at {}",
        blob_store.display()
    );

    let report = report(&output.stdout);
    assert_eq!(
        report
            .get("config_source")
            .and_then(serde_json::Value::as_str),
        Some("(none)")
    );
    assert_eq!(
        report
            .get("control_url")
            .and_then(serde_json::Value::as_str),
        Some(control_url)
    );
    assert_eq!(
        report.get("blob_store").and_then(serde_json::Value::as_str),
        Some(blob_store_arg)
    );
}

#[test]
fn check_config_reports_whether_an_enroller_credential_is_configured() {
    // Presence only: a dry run never opens the credential, so a path that
    // does not exist still reports as configured.
    const KEY: &str = "enroller_file_configured";
    let configured = |output: &Output| {
        assert_success(output);
        report(&output.stdout).get(KEY).cloned()
    };

    // The control: nothing supplies the credential, so a report that always
    // said `true` fails here.
    let unset = run(&["--check-config", "--check-config-format", "json"]);
    assert_eq!(configured(&unset), Some(serde_json::Value::Bool(false)));

    let flagged = run(&[
        "--check-config",
        "--check-config-format",
        "json",
        "--enroller-file",
        "/flag/worker-enroller.json",
    ]);
    assert_eq!(configured(&flagged), Some(serde_json::Value::Bool(true)));

    let from_env = Command::new(env!("CARGO_BIN_EXE_zeroship-worker"))
        .env_clear()
        .env("ZEROSHIP_CONTROL_KEY", STRONG_HEX)
        .env("ZEROSHIP_WORKER_ENROLLER_FILE", "/env/worker-enroller.json")
        .args(["--check-config", "--check-config-format", "json"])
        .output()
        .expect("spawn zeroship-worker");
    assert_eq!(configured(&from_env), Some(serde_json::Value::Bool(true)));
}

#[test]
fn worker_rejects_shared_overlay_selectors() {
    for args in [
        ["--check-config", "--config", "ignored.toml"].as_slice(),
        ["--check-config", "--no-config"].as_slice(),
    ] {
        let output = run(args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "unexpected status for {args:?}:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("unexpected argument"),
            "unexpected clap diagnostic for {args:?}:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn check_config_rejects_an_unknown_report_format() {
    let output = run(&["--check-config", "--check-config-format", "yaml"]);

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid value 'yaml'") && stderr.contains("--check-config-format"),
        "unexpected clap diagnostic:\n{stderr}"
    );
}
