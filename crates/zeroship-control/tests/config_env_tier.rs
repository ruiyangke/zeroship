//! Control-plane config precedence, asserted against the REAL binary.
//!
//! WHY THIS IS A CHILD PROCESS AND NOT A UNIT TEST. The environment tier of
//! `ControlSettings` is clap's `env = "ZEROSHIP_..."` attribute, read during
//! `try_parse_from`. There is no seam to inject a value into: driving that
//! tier means the variable is present in the process clap parses in. The
//! in-process version of these assertions therefore called
//! `std::env::set_var` / `remove_var`, which mutates the environment of every
//! OTHER test in the same binary and, since Rust 2024, is `unsafe` because it
//! races with any concurrent `getenv`. `crates/zeroship-control/src/main.rs` recorded
//! that hazard as measured: 1 failure in 12 runs of that test binary before a
//! mutex was added around the mutation.
//!
//! Spawning the binary with [`Command::env`] scopes the environment to the
//! child, so no sibling test can see it and no mutex is needed. It is also
//! more faithful: `--check-config` runs the shipped resolver over the real
//! CLI, environment and overlay tiers.
//!
//! WHAT THIS DOES NOT COVER: settings absent from `CheckConfigReport`. The
//! report is the observation surface here, so a tier bug in an unreported
//! field is invisible. The cross-BINARY half - control and auth resolving the
//! same shared variable identically - is `tests/config_check_e2e.sh`, the only
//! vector that can watch two processes at once.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Strong hex material for the key-strength guards a dry run still applies.
const STRONG_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Native provider is refused without an explicit OP issuer, before the report
/// is built, so every case supplies one.
const PLATFORM_ISSUER: &str = "http://platform.test/oauth2";

/// A temp directory that removes itself even when an assertion panics.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "zeroship_control_env_tier_{tag}_{}",
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
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run the real `zeroship-control` under `--check-config` with a fully
/// declared environment, returning the raw outcome.
///
/// `env_clear` first: an inherited `ZEROSHIP_*` value from the caller's shell
/// reaches the same clap carrier these fixtures do, so a helper that only
/// ADDED variables would let the developer's shell decide the result. Clearing
/// makes the child's environment exactly the list at each call site.
fn run(overlay: Option<&Path>, extra_env: &[(&str, &str)], args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zeroship-control"));
    cmd.env_clear()
        .env("ZEROSHIP_CONTROL_KEY", STRONG_HEX)
        .env("ZEROSHIP_WORKER_KEY", STRONG_HEX)
        .env("ZEROSHIP_PAIRWISE_SALT", STRONG_HEX)
        .env("ZEROSHIP_CONTROL_MASTER_KEY", STRONG_HEX);
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    cmd.arg("--check-config")
        .arg("--check-config-format")
        .arg("json")
        .arg("--auth-platform-issuer")
        .arg(PLATFORM_ISSUER);
    if let Some(path) = overlay {
        cmd.arg("--config").arg(path);
    }
    cmd.args(args).output().expect("spawn zeroship-control")
}

/// The successful form of [`run`]: the resolved report as JSON text.
fn report(overlay: Option<&Path>, extra_env: &[(&str, &str)], args: &[&str]) -> String {
    let output = run(overlay, extra_env, args);
    assert!(
        output.status.success(),
        "zeroship-control --check-config exited {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf8 --check-config output")
}

/// Pull one reported field out of the JSON report.
///
/// Panics on a missing key rather than returning `None`: absence means the
/// report no longer carries the field these assertions observe, and reporting
/// that as an ordinary value mismatch would hide a moved observation surface.
fn field(text: &str, key: &str) -> String {
    // The report is one JSON object on its own line; a `--check-config` run can
    // also emit structured tracing lines, so take the line that parses as an
    // object carrying the key rather than assuming position.
    for line in text.lines() {
        let Ok(serde_json::Value::Object(map)) = serde_json::from_str(line.trim()) else {
            continue;
        };
        if let Some(value) = map.get(key) {
            return match value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
        }
    }
    panic!("no --check-config JSON line carried a {key} field:\n{text}");
}

#[test]
fn the_overlay_supplies_origin_scheme_when_nothing_outranks_it() {
    // THE CONTROL for the two cases below. Without it, "the environment wins"
    // is satisfiable by a resolver that never reads the overlay at all, and
    // the file half of the precedence claim would be asserted by nothing.
    let scratch = Scratch::new("overlay_only");
    let overlay = scratch.write("control.toml", "origin_scheme = \"https\"\n");

    assert_eq!(field(&report(Some(&overlay), &[], &[]), "origin_scheme"), "https");
}

#[test]
fn the_environment_outranks_the_overlay_and_the_flag_outranks_both() {
    let scratch = Scratch::new("precedence");
    let overlay = scratch.write("control.toml", "origin_scheme = \"https\"\n");

    let from_env = report(
        Some(&overlay),
        &[("ZEROSHIP_ORIGIN_SCHEME", "http")],
        &[],
    );
    assert_eq!(field(&from_env, "origin_scheme"), "http");

    let from_flag = report(
        Some(&overlay),
        &[("ZEROSHIP_ORIGIN_SCHEME", "http")],
        &["--origin-scheme", "https"],
    );
    assert_eq!(field(&from_flag, "origin_scheme"), "https");
}

#[test]
fn the_auth_provider_defaults_to_native_and_the_flag_selects_supabase() {
    // The default half is the reason this test cannot run in-process: clap
    // reads `ZEROSHIP_AUTH_PROVIDER` into the same carrier the flag uses, so
    // an ambient value - a sibling test's, or the caller's shell - would make
    // "defaults to native" an assertion about the environment rather than
    // about the compiled default. A cleared child environment settles it.
    assert_eq!(field(&report(None, &[], &[]), "auth_provider"), "native");

    let flagged = report(None, &[], &["--auth-provider", "supabase"]);
    assert_eq!(field(&flagged, "auth_provider"), "supabase");
}

#[test]
fn the_shared_auth_provider_variable_reaches_control_from_the_environment() {
    let from_env = report(None, &[("ZEROSHIP_AUTH_PROVIDER", "supabase")], &[]);
    assert_eq!(field(&from_env, "auth_provider"), "supabase");
}

#[test]
fn the_retired_platform_spelling_is_rejected_by_the_overlay() {
    // `platform` was control's own word for the state now spelled `native`.
    // The FLAG half of this refusal is in `crates/zeroship-control/src/main.rs` - clap
    // rejects an explicit unknown value without consulting the environment, so
    // it needs no child process. The OVERLAY half does: the environment tier
    // outranks the overlay, so an ambient `ZEROSHIP_AUTH_PROVIDER` makes the
    // retired overlay value never get parsed and this refusal never fire.
    let scratch = Scratch::new("retired_spelling");
    let overlay = scratch.write("control.toml", "[auth]\nprovider = \"platform\"\n");

    let output = run(Some(&overlay), &[], &[]);
    assert!(
        !output.status.success(),
        "the retired control spelling must not resolve from the overlay"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("auth.provider"),
        "the overlay rejection must name the key: {combined}"
    );
}

#[test]
fn a_valid_overlay_provider_still_resolves() {
    // The one-variable partner to the refusal above: same key, same table,
    // only the VALUE differs. Without it, a resolver that rejected every
    // `[auth] provider` - or failed for an unrelated reason - would pass the
    // test above while proving nothing about the retired spelling.
    let scratch = Scratch::new("valid_provider");
    let overlay = scratch.write("control.toml", "[auth]\nprovider = \"supabase\"\n");

    assert_eq!(
        field(&report(Some(&overlay), &[], &[]), "auth_provider"),
        "supabase"
    );
}
