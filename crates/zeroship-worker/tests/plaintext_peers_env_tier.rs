//! `ZEROSHIP_PLAINTEXT_PEERS` reaches the transport fence, asserted against the
//! REAL binary.
//!
//! WHY THIS IS A CHILD PROCESS AND NOT A UNIT TEST. The unit checks on the
//! fence prove it would honour a named peer if one arrived. They cannot prove
//! one ARRIVES: the value has to traverse clap, the resolver, the report and
//! the host config before `Transport::configuration` ever sees it, and a
//! relaxation that reached the flag tier but not the environment - or reached a
//! declaration nothing reads - would leave every one of those checks green
//! while a deployed worker still refused its manager.
//!
//! Observing an environment tier requires the variable to be present in the
//! process being observed. `Command::env` scopes it to the child, where
//! `std::env::set_var` would be process-global, seen by every other test in
//! this binary, and `unsafe` since Rust 2024 because it races any concurrent
//! `getenv`.
//!
//! The worker is the cheapest of the three consumers to observe: its manager
//! origin is validated during `--check-config`, so the refusal and the
//! admission are both reachable without a database, a socket or a signer.

use std::process::{Command, Output};

const STRONG_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A plaintext manager origin under a DNS name, which is what a private
/// container network looks like and what the fence refuses by default.
const MANAGER: &str = "http://workflow:9093";

/// The setting a refusal must name. Asserting on it separates "the worker
/// refused this origin" from "the worker failed to run at all" - a missing
/// binary, a renamed flag and a panic all exit non-zero.
const SETTING: &str = "worker.workflow_manager_url";

struct Scratch(tempfile::TempDir);

impl Scratch {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("create worker scratch directory");
        std::fs::write(dir.path().join("worker-dsn"), "postgres://check-config")
            .expect("write the scratch DSN");
        Self(dir)
    }

    fn path(&self, name: &str) -> String {
        self.0
            .path()
            .join(name)
            .to_str()
            .expect("UTF-8 scratch path")
            .to_owned()
    }
}

/// Run the real `zeroship-worker` under `--check-config` with a workflow host
/// configured against `MANAGER`.
///
/// `env_clear` first: an inherited `ZEROSHIP_*` value from the caller's shell
/// reaches the same resolver these fixtures do, so a helper that only ADDED
/// variables would let the developer's shell supply the very list this test
/// asserts is absent.
fn run(scratch: &Scratch, extra_env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zeroship-worker"));
    cmd.env_clear().env("ZEROSHIP_CONTROL_KEY", STRONG_HEX);
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    cmd.args([
        "--check-config",
        "--check-config-format",
        "json",
        "--workflow-manager-url",
        MANAGER,
        "--database-url-file",
        &scratch.path("worker-dsn"),
        "--storage-url",
        &scratch.path("objects"),
    ])
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

/// The report the dry run prints on stdout. Parsed by the key every report
/// carries rather than by position, so a diagnostic line sharing the stream
/// cannot be mistaken for it.
fn report(output: &Output) -> serde_json::Map<String, serde_json::Value> {
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|value| value.as_object().cloned())
        .find(|object| object.contains_key("workflow_host_configured"))
        .unwrap_or_else(|| panic!("no check-config report in stdout:\n{text}"))
}

/// THE BASELINE. With the variable absent, the worker refuses a plaintext
/// manager under a DNS name and names the setting.
#[test]
fn without_the_variable_the_worker_refuses_a_plaintext_non_loopback_manager() {
    let scratch = Scratch::new();
    let output = run(&scratch, &[]);
    assert!(
        !output.status.success(),
        "a plaintext non-loopback manager must be refused by default: {}",
        combined(&output)
    );
    let text = combined(&output);
    assert!(
        text.contains(SETTING),
        "the refusal must name the setting it is about: {text}"
    );
}

/// ONE VARIABLE from the baseline above: `ZEROSHIP_PLAINTEXT_PEERS` naming the
/// manager's own origin. If the environment tier did not reach the fence, this
/// run would be byte-identical to the baseline.
#[test]
fn the_environment_tier_carries_a_named_peer_all_the_way_to_the_fence() {
    let scratch = Scratch::new();
    let output = run(&scratch, &[("ZEROSHIP_PLAINTEXT_PEERS", MANAGER)]);
    assert!(
        output.status.success(),
        "a named plaintext peer must start a workflow host, exited {:?}: {}",
        output.status.code(),
        combined(&output)
    );
    let report = report(&output);
    assert_eq!(
        report.get("workflow_host_configured"),
        Some(&serde_json::Value::Bool(true))
    );
    assert_eq!(
        report
            .get("plaintext_peers")
            .and_then(serde_json::Value::as_str),
        Some(MANAGER),
        "the report states the posture the run is under"
    );
}

/// The list is matched against the ORIGIN, not merely present. Naming a
/// different origin through the same variable leaves the manager refused, so
/// the admission above is the value's doing rather than the variable's
/// presence.
#[test]
fn the_environment_tier_admits_only_the_origin_it_names() {
    let scratch = Scratch::new();
    let output = run(
        &scratch,
        &[("ZEROSHIP_PLAINTEXT_PEERS", "http://control:9090")],
    );
    assert!(
        !output.status.success(),
        "an origin the list does not name must stay refused: {}",
        combined(&output)
    );
    assert!(combined(&output).contains(SETTING));
}

/// An unusable entry is refused while resolving, rather than resolving to a
/// list that quietly admits nothing - which would look exactly like the
/// baseline refusal and would be read as "the fence held".
#[test]
fn the_environment_tier_refuses_an_entry_that_is_not_one_exact_http_origin() {
    let scratch = Scratch::new();
    for entry in [
        // https authorizes nothing here, so accepting it would let an operator
        // believe a peer was listed when the list is about plaintext alone.
        "https://workflow:9093",
        "workflow:9093",
        "http://*.workflow",
        "http://workflow:9093/prefix",
    ] {
        let output = run(&scratch, &[("ZEROSHIP_PLAINTEXT_PEERS", entry)]);
        assert!(
            !output.status.success(),
            "{entry} must be refused as a plaintext peer: {}",
            combined(&output)
        );
        let text = combined(&output);
        assert!(
            text.contains("plaintext"),
            "the refusal must name what it rejected for {entry}: {text}"
        );
    }
}

/// All three supply tiers reach the same fence. A setting that existed only in
/// the environment would leave the flag and file surfaces looking untouched -
/// which is the shape `crates/zeroship-auth/tests/config_env_tier.rs` exists to
/// catch - so the flag tier is driven here against the same origin, and the
/// file tier is driven through the workflow manager's overlay in
/// `crates/zeroship-workflow-server/tests/config.rs`. The worker itself has no
/// TOML overlay by design.
#[test]
fn the_flag_tier_carries_the_same_named_peer_to_the_same_fence() {
    let scratch = Scratch::new();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zeroship-worker"));
    let output = cmd
        .env_clear()
        .env("ZEROSHIP_CONTROL_KEY", STRONG_HEX)
        .args([
            "--check-config",
            "--check-config-format",
            "json",
            "--workflow-manager-url",
            MANAGER,
            "--database-url-file",
            &scratch.path("worker-dsn"),
            "--storage-url",
            &scratch.path("objects"),
            "--plaintext-peers",
            MANAGER,
        ])
        .output()
        .expect("spawn zeroship-worker");
    assert!(
        output.status.success(),
        "the flag tier must admit the same origin, exited {:?}: {}",
        output.status.code(),
        combined(&output)
    );
    assert_eq!(
        report(&output)
            .get("plaintext_peers")
            .and_then(serde_json::Value::as_str),
        Some(MANAGER)
    );
}
