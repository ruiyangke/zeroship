//! The obsolete security-relaxation variable, asserted against the REAL binary.
//!
//! WHY THIS IS A CHILD PROCESS AND NOT A UNIT TEST. The claim is that
//! `ZEROSHIP_DEV_INSECURE` reaches NO supply tier: it was the environment twin
//! of a deleted `--dev-insecure` flag, and a relaxation that survived only in
//! the environment would leave the flag surface untouched while still weakening
//! a running auth service. The only way to observe an environment tier is for
//! the variable to be present in the process being observed, so the in-process
//! version of this test called `std::env::set_var` - process-global, seen by
//! every other test in the binary, and `unsafe` since Rust 2024 because it
//! races with any concurrent `getenv`.
//!
//! `Command::env` scopes the variable to the child. The assertion also got
//! stronger in the move: the in-process test could only ask
//! `Secret::is_configured()`, while this one drives the real startup guard and
//! observes the process refusing to boot.

use std::process::{Command, Output};

/// Strong hex material for the key-strength guards.
const STRONG_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The label `validate_stash_key` puts in its refusal. Asserting on it is what
/// separates "auth refused this configuration" from "auth failed to run at
/// all" - a missing binary, a renamed flag and a panic all exit non-zero.
const STASH_KEY_LABEL: &str = "ZEROSHIP_AUTH_STASH_SIGNING_KEY";

/// Run the real `zeroship-auth` under `--check-config`.
///
/// `env_clear` first: an inherited `ZEROSHIP_AUTH_*` value from the caller's
/// shell reaches the same resolver these fixtures do, so a helper that only
/// ADDED variables would let the developer's shell supply the very keys this
/// test asserts are absent.
fn run(extra_env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zeroship-auth"));
    cmd.env_clear()
        .env("ZEROSHIP_AUTH_DATABASE_URL", "postgres://check-config");
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    cmd.arg("--check-config")
        .output()
        .expect("spawn zeroship-auth")
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn auth_refuses_to_start_without_a_stash_signing_key() {
    // THE BASELINE. Establishes what the refusal looks like when the obsolete
    // variable is absent, so the case below differs from it in exactly one
    // variable.
    let output = run(&[]);
    assert!(
        !output.status.success(),
        "auth must refuse an unsupplied stash signing key"
    );
    let text = combined(&output);
    assert!(
        text.contains(STASH_KEY_LABEL),
        "the refusal must name the key it wants: {text}"
    );
}

#[test]
fn the_obsolete_dev_insecure_variable_turns_no_supply_tier_on() {
    // ONE VARIABLE from the baseline above: `ZEROSHIP_DEV_INSECURE=1`. If it
    // still reached any tier, the guard would be relaxed or the keys would be
    // considered supplied, and this run would differ.
    let output = run(&[("ZEROSHIP_DEV_INSECURE", "1")]);
    assert!(
        !output.status.success(),
        "an obsolete relaxation variable must not let auth start without its keys"
    );
    let text = combined(&output);
    assert!(
        text.contains(STASH_KEY_LABEL),
        "the refusal must still name the key it wants: {text}"
    );
}

#[test]
fn auth_starts_a_dry_run_once_the_keys_are_supplied() {
    // The partner that stops the two refusals above being vacuous: with the
    // keys present the SAME command line and the SAME obsolete variable reach
    // exit 0, so the refusals are about the missing keys and not about auth
    // being unable to run under a cleared environment.
    let output = run(&[
        ("ZEROSHIP_DEV_INSECURE", "1"),
        ("ZEROSHIP_AUTH_STASH_SIGNING_KEY", STRONG_HEX),
        ("ZEROSHIP_AUTH_TOTP_ENC_KEY", STRONG_HEX),
    ]);
    assert!(
        output.status.success(),
        "auth --check-config exited {:?}: {}",
        output.status.code(),
        combined(&output)
    );
}
