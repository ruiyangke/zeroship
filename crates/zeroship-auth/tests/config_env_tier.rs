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

#[test]
fn the_shared_auth_provider_variable_reaches_auth() {
    let output = run(&[
        ("ZEROSHIP_AUTH_STASH_SIGNING_KEY", STRONG_HEX),
        ("ZEROSHIP_AUTH_TOTP_ENC_KEY", STRONG_HEX),
        ("ZEROSHIP_AUTH_PROVIDER", "supabase"),
        ("ZEROSHIP_AUTH_SUPABASE_URL", "https://project.supabase.co"),
        ("ZEROSHIP_AUTH_SUPABASE_ANON_KEY", "anon"),
    ]);

    assert!(
        output.status.success(),
        "auth --check-config exited {:?}: {}",
        output.status.code(),
        combined(&output)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("auth_provider = supabase"),
        "auth did not report the provider supplied through the shared variable: {}",
        combined(&output)
    );
}

/// Run the real `zeroship-auth` under `--check-config` with extra ARGUMENTS as
/// well as extra variables, so the flag tier can be driven too.
fn run_with_args(extra_env: &[(&str, &str)], args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zeroship-auth"));
    cmd.env_clear()
        .env("ZEROSHIP_AUTH_DATABASE_URL", "postgres://check-config");
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    cmd.arg("--check-config")
        .args(args)
        .output()
        .expect("spawn zeroship-auth")
}

/// The keys every dry run below needs before it reaches a report at all.
const KEYS: [(&str, &str); 2] = [
    ("ZEROSHIP_AUTH_STASH_SIGNING_KEY", STRONG_HEX),
    ("ZEROSHIP_AUTH_TOTP_ENC_KEY", STRONG_HEX),
];

/// The one-per-core count `auth.threads` resolves to when nothing supplies it.
///
/// Recomputed here rather than written down: the compiled default is
/// `zeroship_core::config::default_http_threads`, and a literal would pin this
/// test to the machine that wrote it.
fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
}

#[test]
fn the_unset_thread_count_still_resolves_to_one_per_core() {
    // THE ARM THAT MATTERS MOST. `auth.threads` exists so an operator can spend
    // fewer io_uring rings on a host with a small `ulimit -l`; the price of
    // getting its default wrong is a silent concurrency change on every
    // deployment that never sets it. Asserting only the explicit arm below
    // would pass over a default that had quietly become 1.
    let output = run_with_args(&KEYS, &["--no-config"]);
    assert!(
        output.status.success(),
        "auth --check-config exited {:?}: {}",
        output.status.code(),
        combined(&output)
    );
    let expected = format!("threads = {}", cores());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&expected),
        "the report must carry {expected:?}: {}",
        combined(&output)
    );
}

#[test]
fn an_explicit_thread_count_reaches_the_report_from_flag_and_environment() {
    let mut env = KEYS.to_vec();
    env.push(("ZEROSHIP_AUTH_THREADS", "3"));

    let from_env = run_with_args(&env, &["--no-config"]);
    assert!(
        from_env.status.success(),
        "auth --check-config exited {:?}: {}",
        from_env.status.code(),
        combined(&from_env)
    );
    assert!(
        String::from_utf8_lossy(&from_env.stdout).contains("threads = 3"),
        "the environment tier did not reach the report: {}",
        combined(&from_env)
    );

    // The flag outranks the environment.
    let from_flag = run_with_args(&env, &["--no-config", "--threads", "2"]);
    assert!(
        from_flag.status.success(),
        "auth --check-config exited {:?}: {}",
        from_flag.status.code(),
        combined(&from_flag)
    );
    assert!(
        String::from_utf8_lossy(&from_flag.stdout).contains("threads = 2"),
        "the flag tier did not outrank the environment: {}",
        combined(&from_flag)
    );

    // The one-variable control: the same keys, neither tier supplying a count,
    // resolves the compiled default instead of echoing 2 or 3.
    let bare = run_with_args(&KEYS, &["--no-config"]);
    let expected = format!("threads = {}", cores());
    assert!(
        String::from_utf8_lossy(&bare.stdout).contains(&expected),
        "the control run must carry {expected:?}: {}",
        combined(&bare)
    );
}

#[test]
fn a_zero_thread_count_is_refused_by_the_dry_run() {
    // ntex does not clamp: zero arbiters means the process binds its port,
    // passes a TCP liveness probe and answers nothing. The dry run has to
    // refuse it, because the dry run is where an operator finds out.
    let output = run_with_args(&KEYS, &["--no-config", "--threads", "0"]);

    assert!(
        !output.status.success(),
        "zero serving threads must refuse; the run exited {:?}: {}",
        output.status.code(),
        combined(&output)
    );
    assert!(
        combined(&output).contains("auth.threads"),
        "the refusal must name the setting an operator can change: {}",
        combined(&output)
    );
}
