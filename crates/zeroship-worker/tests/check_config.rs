//! Process-level configuration checks for the worker binary.
//!
//! The worker deliberately has no shared overlay. These tests launch Cargo's
//! freshly built binary so the assertion covers the shipped argument parser,
//! resolver, report, and early return without mutating the test process's
//! environment.

use std::process::{Command, Output};

use zeroship_core::config::{REMEDIATION_COMMAND, SERVICE_CREDENTIAL_SENTINEL};

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
fn check_config_reports_whether_a_join_token_is_configured() {
    // Presence only: a dry run never opens the token, so a path that does not
    // exist still reports as configured.
    const KEY: &str = "join_token_file_configured";
    let configured = |output: &Output| {
        assert_success(output);
        report(&output.stdout).get(KEY).cloned()
    };

    // The control: nothing supplies the token, so a report that always said
    // `true` fails here.
    let unset = run(&["--check-config", "--check-config-format", "json"]);
    assert_eq!(configured(&unset), Some(serde_json::Value::Bool(false)));

    let flagged = run(&[
        "--check-config",
        "--check-config-format",
        "json",
        "--join-token-file",
        "/flag/worker-join-token",
    ]);
    assert_eq!(configured(&flagged), Some(serde_json::Value::Bool(true)));

    let from_env = Command::new(env!("CARGO_BIN_EXE_zeroship-worker"))
        .env_clear()
        .env("ZEROSHIP_CONTROL_KEY", STRONG_HEX)
        .env("ZEROSHIP_WORKER_JOIN_TOKEN_FILE", "/env/worker-join-token")
        .args(["--check-config", "--check-config-format", "json"])
        .output()
        .expect("spawn zeroship-worker");
    assert_eq!(configured(&from_env), Some(serde_json::Value::Bool(true)));
}

/// A refusal exiting `code` whose diagnostic names `setting`.
fn assert_refusal(output: &Output, code: i32, setting: &str) {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(code), "{text}");
    assert!(
        text.contains(setting),
        "the refusal must name {setting}:\n{text}"
    );
}

/// `--check-config` with the workflow host settings under test.
fn workflow_host_dry_run(settings: &[&str]) -> Output {
    let mut args = vec!["--check-config", "--check-config-format", "json"];
    args.extend_from_slice(settings);
    run(&args)
}

/// The workflow host is optional, and a configured one must be able to run.
///
/// Every manager exchange is signed with this instance's enrolled key, so the
/// origin is part of that credential's trust boundary; and the host prepares
/// creator journals in the worker's own database and stages payloads in its own
/// object store. A worker that bound its port and then registered capacity it
/// could never serve would strand every placement the manager gave it, so each
/// of these is a refusal, not a warning. `--check-config` reaches the same gate,
/// which is why the refusals are observable without binding anything.
#[test]
fn a_workflow_host_is_refused_without_a_usable_manager_origin_or_creator_resources() {
    let scratch = tempfile::tempdir().expect("create workflow host scratch directory");
    let dsn = scratch.path().join("worker-dsn");
    let dsn_arg = dsn.to_str().expect("UTF-8 scratch path");
    let objects = scratch.path().join("objects");
    let objects_arg = objects.to_str().expect("UTF-8 scratch path");
    let loopback = "http://127.0.0.1:9095";

    // A plaintext origin anywhere but this machine would hand the instance's
    // signed assertions to the network.
    assert_refusal(
        &workflow_host_dry_run(&["--workflow-manager-url", "http://workflow.example"]),
        2,
        "worker.workflow_manager_url",
    );

    // A usable origin still needs both creator resources.
    assert_refusal(
        &workflow_host_dry_run(&["--workflow-manager-url", loopback]),
        1,
        "worker.database_url",
    );
    assert_refusal(
        &workflow_host_dry_run(&[
            "--workflow-manager-url",
            loopback,
            "--database-url-file",
            dsn_arg,
        ]),
        1,
        "worker.storage_url",
    );

    // The control: with both resources the dry run reports the host it would
    // start, carrying the bounds it was given rather than the defaults.
    let configured = workflow_host_dry_run(&[
        "--workflow-manager-url",
        loopback,
        "--database-url-file",
        dsn_arg,
        "--storage-url",
        objects_arg,
        "--workflow-capacity",
        "12",
        "--workflow-slots",
        "3",
    ]);
    assert_success(&configured);
    let host = report(&configured.stdout);
    assert_eq!(
        host.get("workflow_host_configured"),
        Some(&serde_json::Value::Bool(true))
    );
    assert_eq!(
        host.get("workflow_manager_url")
            .and_then(serde_json::Value::as_str),
        Some(loopback)
    );
    assert_eq!(
        host.get("workflow_capacity")
            .and_then(serde_json::Value::as_u64),
        Some(12)
    );
    assert_eq!(
        host.get("workflow_slots")
            .and_then(serde_json::Value::as_u64),
        Some(3)
    );

    // The second control: the default deployment runs no host at all, so a
    // report that always said `true` fails here.
    let unset = workflow_host_dry_run(&[]);
    assert_success(&unset);
    assert_eq!(
        report(&unset.stdout).get("workflow_host_configured"),
        Some(&serde_json::Value::Bool(false))
    );
    // The plaintext-peer posture is reported EMPTY here, not absent. A field
    // that only appeared once the fence was relaxed could never be read as
    // "the fence is intact".
    assert_eq!(
        report(&unset.stdout)
            .get("plaintext_peers")
            .and_then(serde_json::Value::as_str),
        Some("")
    );
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

/// A dry run with the control key under test.
fn dry_run_with(key: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zeroship-worker"))
        .env_clear()
        .env("ZEROSHIP_CONTROL_KEY", key)
        .args(["--check-config", "--check-config-format", "json"])
        .output()
        .expect("spawn zeroship-worker")
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Two runs of one refusal are never byte-identical as emitted because the
/// tracing lines carry a wall-clock timestamp. The identity under test is the
/// MESSAGE, so drop the timestamp and compare what is left.
fn normalised(text: &str) -> String {
    text.lines()
        .map(|line| match serde_json::from_str::<serde_json::Value>(line) {
            Ok(mut value) => {
                if let Some(object) = value.as_object_mut() {
                    object.remove("timestamp");
                }
                value.to_string()
            }
            Err(_) => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The worker's control key is floorless - `SecretStrength::Unrestricted` - so
/// no length floor can refuse a placeholder. Only the sentinel branch can, and
/// that is the case this test exists for.
#[test]
fn a_placeholder_control_key_is_refused_on_a_floorless_credential() {
    let output = dry_run_with(SERVICE_CREDENTIAL_SENTINEL);

    assert!(
        !output.status.success(),
        "the worker accepted the {SERVICE_CREDENTIAL_SENTINEL} placeholder on a floorless \
         credential"
    );

    let banner = combined(&output);
    for token in [SERVICE_CREDENTIAL_SENTINEL, REMEDIATION_COMMAND, "REFUSES TO START"] {
        assert!(
            banner.contains(token),
            "the sentinel banner does not name {token:?}:\n{banner}"
        );
    }
}

#[test]
fn an_empty_control_key_is_refused_in_the_same_words_as_the_sentinel() {
    let sentinel = dry_run_with(SERVICE_CREDENTIAL_SENTINEL);
    let empty = dry_run_with("");

    assert!(
        !empty.status.success(),
        "the worker accepted an EMPTY floorless credential"
    );

    // Not "both failed" but "both failed in the same words": a build giving the
    // empty case its own branch would satisfy a both-failed check and fail this.
    assert_eq!(
        normalised(&combined(&sentinel)),
        normalised(&combined(&empty)),
        "empty and sentinel must reach ONE branch"
    );
}

/// One byte of real material in the same floorless credential. This is the
/// control that proves the refusals above are about the placeholder and not
/// about length: `require_nonempty` has no floor to fail.
#[test]
fn one_byte_of_real_material_is_accepted_on_a_floorless_credential() {
    let output = dry_run_with("x");

    assert_success(&output);
    assert!(!combined(&output).contains("REFUSES TO START"));
}
