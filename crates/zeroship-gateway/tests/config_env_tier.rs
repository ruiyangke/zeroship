//! Gateway config precedence, asserted against the REAL binary.
//!
//! WHY THIS IS A CHILD PROCESS AND NOT A UNIT TEST. The environment tier of
//! `GateSettings` is clap's `env = "ZEROSHIP_..."` attribute, read by clap
//! during `try_parse_from`. There is no seam to inject a value into: the only
//! way to drive that tier is for the variable to be present in the process
//! clap is parsing in. The in-process version of these assertions therefore
//! called `std::env::set_var`, which mutates the environment of every OTHER
//! test in the same binary and, since Rust 2024, is `unsafe` because it races
//! with any concurrent `getenv`.
//!
//! Spawning the binary with [`Command::env`] moves the same assertion onto a
//! process whose environment is an explicit argument at this call site. It is
//! also strictly MORE faithful: `--check-config` runs the real resolver over
//! the real CLI, environment and overlay tiers and prints what a real launch
//! would use, so this observes the shipped precedence rather than a
//! reconstruction of it.
//!
//! WHAT THIS DOES NOT COVER: the settings `--check-config` does not report.
//! The report is the observation surface, so a tier bug in a field that is not
//! in `CheckConfigReport` is invisible here. `crates/zeroship-gateway/src/main.rs` is
//! where that list lives.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Strong hex material for the key-strength guards `--check-config` runs.
const STRONG_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A temp directory that removes itself even when an assertion panics.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "zeroship_gate_env_tier_{tag}_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        Self(path)
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, contents).expect("write fixture");
        path
    }

    /// A file the secret resolver will accept: owner-only, as it refuses any
    /// secret a second local account could read.
    fn write_secret(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.write(name, contents);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("owner-only fixture secret");
        }
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One `--check-config` run of the real `zeroship-gate`, with a fully declared
/// environment.
///
/// `env_clear` first: an inherited `ZEROSHIP_*` value from the caller's shell
/// reaches the same clap carrier the fixtures below do, so a test that only
/// ADDED variables would be asserting about the developer's shell whenever one
/// happened to be exported. Clearing makes the environment of the process
/// under test exactly the list at each call site and nothing else.
fn check_config(broker_secret: &Path, overlay: &Path, extra_env: &[(&str, &str)], args: &[&str]) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zeroship-gate"));
    cmd.env_clear()
        .env("ZEROSHIP_CONTROL_KEY", STRONG_HEX)
        .env("ZEROSHIP_GATEWAY_STASH_SIGNING_KEY", STRONG_HEX)
        .env("ZEROSHIP_PAIRWISE_SALT", STRONG_HEX);
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    let output = cmd
        .arg("--check-config")
        .arg("--check-config-format")
        .arg("json")
        .arg("--config")
        .arg(overlay)
        .arg("--broker-secret-file")
        .arg(broker_secret)
        .args(args)
        .output()
        .expect("spawn zeroship-gate");

    assert!(
        output.status.success(),
        "zeroship-gate --check-config exited {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf8 --check-config output")
}

/// Pull one reported field out of the JSON report.
///
/// The report is one JSON object on its own line, but a `--check-config` run
/// also emits structured tracing lines on stdout (the overlay it loaded, the
/// signing key it did not find), so the report is found by CONTENT rather than
/// by position. Panics on a missing key rather than returning `None`: absence
/// means the report no longer carries the field these assertions observe, and
/// reporting that as an ordinary value mismatch would hide a moved
/// observation surface.
fn field(report: &str, key: &str) -> String {
    for line in report.lines() {
        let Ok(serde_json::Value::Object(map)) = serde_json::from_str(line.trim()) else {
            continue;
        };
        if let Some(value) = map.get(key) {
            return match value {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            };
        }
    }
    panic!("no --check-config JSON line carried a {key} field:\n{report}");
}

/// The overlay both cases below read. It names BOTH settings with values that
/// differ from every environment and flag value used here, so a report that
/// echoed the file would be unmistakable.
const OVERLAY: &str = concat!(
    "origin_scheme = \"https\"\n",
    "trusted_origins = [\"https://file.example\"]\n",
);

#[test]
fn the_overlay_supplies_topology_when_nothing_outranks_it() {
    // THE CONTROL for the two cases below. Without it, "the environment wins"
    // and "the flag wins" are both satisfiable by a resolver that ignores the
    // overlay entirely, and the file half of the precedence claim would be
    // asserted by nothing.
    let scratch = Scratch::new("overlay_only");
    let secret = scratch.write_secret("broker", "gateway-broker-secret-32-bytes-minimum-ok");
    let overlay = scratch.write("gateway.toml", OVERLAY);

    let report = check_config(&secret, &overlay, &[], &[]);
    assert_eq!(field(&report, "origin_scheme"), "https");
    assert_eq!(field(&report, "trusted_origins"), "https://file.example");
}

#[test]
fn the_environment_outranks_the_overlay() {
    let scratch = Scratch::new("env_over_file");
    let secret = scratch.write_secret("broker", "gateway-broker-secret-32-bytes-minimum-ok");
    let overlay = scratch.write("gateway.toml", OVERLAY);

    let report = check_config(
        &secret,
        &overlay,
        &[
            ("ZEROSHIP_ORIGIN_SCHEME", "http"),
            (
                "ZEROSHIP_TRUSTED_ORIGINS",
                "https://env.example,http://localhost:3000",
            ),
        ],
        &[],
    );

    assert_eq!(field(&report, "origin_scheme"), "http");
    // Values and ORDER, not a count: a resolver that kept the right number of
    // origins from the wrong tier would pass a count assertion.
    assert_eq!(
        field(&report, "trusted_origins"),
        "https://env.example,http://localhost:3000"
    );
}

#[test]
fn the_flag_outranks_both_the_environment_and_the_overlay() {
    let scratch = Scratch::new("cli_over_env");
    let secret = scratch.write_secret("broker", "gateway-broker-secret-32-bytes-minimum-ok");
    let overlay = scratch.write("gateway.toml", OVERLAY);

    let report = check_config(
        &secret,
        &overlay,
        &[
            ("ZEROSHIP_ORIGIN_SCHEME", "http"),
            (
                "ZEROSHIP_TRUSTED_ORIGINS",
                "https://env.example,http://localhost:3000",
            ),
        ],
        &[
            "--origin-scheme",
            "https",
            "--trusted-origins",
            "https://cli.example",
        ],
    );

    assert_eq!(field(&report, "origin_scheme"), "https");
    assert_eq!(field(&report, "trusted_origins"), "https://cli.example");
}

#[test]
fn the_obsolete_security_relaxation_variable_reaches_no_carrier() {
    // `--dev-insecure` was deleted. The flag half of that is asserted in
    // `crates/zeroship-gateway/src/main.rs` (clap rejects an unknown argument without
    // consulting the environment at all); this is the tier the flag test
    // cannot see, because an env-only carrier would leave the flag surface
    // untouched and still relax the gateway.
    let scratch = Scratch::new("dev_insecure");
    let secret = scratch.write_secret("broker", "gateway-broker-secret-32-bytes-minimum-ok");
    let overlay = scratch.write("gateway.toml", OVERLAY);

    let report = check_config(
        &secret,
        &overlay,
        &[("ZEROSHIP_DEV_INSECURE", "1")],
        &[],
    );

    // Unchanged from the overlay-only control above: the variable moved
    // nothing.
    assert_eq!(field(&report, "origin_scheme"), "https");
    assert_eq!(field(&report, "trust_proxy"), "false");
}
