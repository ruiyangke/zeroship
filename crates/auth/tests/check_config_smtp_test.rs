//! ISS-62 regression: `zeroship-auth --check-config` must resolve + report the
//! config and exit 0 BEFORE any runtime-only validation (the relay-forward /
//! transactional mailer SMTP-host requirement). It is a config DRY-RUN — it must
//! not be gated by boot-time mailer construction, exactly like control / gateway
//! / worker. A NORMAL boot (no `--check-config`) must STILL fail fast on a
//! missing `AUTH_RELAY_SMTP_HOST` so the real boot path is not weakened.
//!
//! These are process-level: `--check-config` exits inside `main` before the DB /
//! server come up, so no live services are needed. We drive the compiled
//! binary via `CARGO_BIN_EXE_zeroship-auth` (provided to integration test
//! targets) under a wiped env so a stray `AUTH_*` in the dev shell can't taint
//! the assertion.

use std::process::Command;

/// Path to the freshly-built `zeroship-auth` binary (cargo provides this env var
/// to integration test targets).
fn auth_bin() -> &'static str {
    env!("CARGO_BIN_EXE_zeroship-auth")
}

/// Run the binary with a wiped environment (only PATH/HOME survive, mirroring the
/// `env -i` discipline in `tests/config_check_e2e.sh`), capturing status+stdout.
fn run_auth(args: &[&str]) -> (std::process::ExitStatus, String, String) {
    let path = std::env::var("PATH").unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    let out = Command::new(auth_bin())
        .args(args)
        .env_clear()
        .env("PATH", path)
        .env("HOME", home)
        .output()
        .expect("spawn zeroship-auth");
    (
        out.status,
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// RED before the fix: with the default `--relay-forward-mailer=smtp` and NO
/// `AUTH_RELAY_SMTP_HOST`, the build-relay-mailer validation ran BEFORE the
/// `--check-config` short-circuit, so the dry-run exited 1 with
/// `AUTH_RELAY_SMTP_HOST is required …` and never printed the resolved config.
/// After the fix the dry-run resolves + reports the config and exits 0.
#[test]
fn check_config_short_circuits_before_relay_smtp_validation() {
    let (status, stdout, stderr) = run_auth(&[
        "--check-config",
        "--db-url",
        "postgres://check-config",
        "--dev-insecure",
        // Explicit default — make the SMTP-host requirement unambiguous.
        "--relay-forward-mailer=smtp",
    ]);

    assert!(
        status.success(),
        "--check-config must exit 0 BEFORE the relay-SMTP validation \
         (a config dry-run is not gated by runtime-only mailer construction).\n\
         status={status:?}\nstdout=\n{stdout}\nstderr=\n{stderr}"
    );
    // The runtime-only SMTP requirement must NOT have fired in dry-run mode.
    assert!(
        !stderr.contains("AUTH_RELAY_SMTP_HOST is required"),
        "the relay-SMTP requirement must not gate --check-config; stderr=\n{stderr}"
    );
    // The resolved config report must actually be emitted (it is the whole point
    // of the dry-run). The report prints `auth_provider = …` on stdout.
    assert!(
        stdout.contains("auth_provider = native"),
        "--check-config must print the resolved config report; stdout=\n{stdout}"
    );
}

/// The transactional mailer SMTP-host requirement must likewise not gate the
/// dry-run: `--mailer=smtp` with no `AUTH_SMTP_HOST` still exits 0 under
/// `--check-config`.
#[test]
fn check_config_short_circuits_before_transactional_smtp_validation() {
    let (status, stdout, stderr) = run_auth(&[
        "--check-config",
        "--db-url",
        "postgres://check-config",
        "--dev-insecure",
        "--mailer=smtp",
        // Relay set to stdout so ONLY the transactional SMTP requirement is in play.
        "--relay-forward-mailer=stdout",
    ]);

    assert!(
        status.success(),
        "--check-config must exit 0 even with --mailer=smtp and no AUTH_SMTP_HOST.\n\
         status={status:?}\nstdout=\n{stdout}\nstderr=\n{stderr}"
    );
    assert!(
        !stderr.contains("AUTH_SMTP_HOST is required"),
        "the transactional SMTP requirement must not gate --check-config; stderr=\n{stderr}"
    );
    assert!(
        stdout.contains("mailer = smtp"),
        "--check-config must report the resolved mailer driver; stdout=\n{stdout}"
    );
}

/// The fix must NOT weaken the REAL boot path: a normal boot (no
/// `--check-config`) with `--relay-forward-mailer=smtp` and no
/// `AUTH_RELAY_SMTP_HOST` must STILL fail fast with the named env var, before any
/// DB work. We assert it exits non-zero AND emits the exact requirement
/// message — proving the runtime validation was merely relocated past the
/// dry-run early-return, not deleted.
#[test]
fn real_boot_still_enforces_relay_smtp_host() {
    let (status, stdout, stderr) = run_auth(&[
        // NOTE: no --check-config — this is the real boot path.
        "--db-url",
        "postgres://127.0.0.1:1/unreachable-on-purpose",
        "--dev-insecure",
        "--relay-forward-mailer=smtp",
    ]);

    assert!(
        !status.success(),
        "a real boot with --relay-forward-mailer=smtp and no AUTH_RELAY_SMTP_HOST \
         must fail fast.\nstatus={status:?}\nstdout=\n{stdout}\nstderr=\n{stderr}"
    );
    assert!(
        stderr.contains("AUTH_RELAY_SMTP_HOST is required")
            || stdout.contains("AUTH_RELAY_SMTP_HOST is required"),
        "the real boot must still surface the named SMTP-host requirement \
         (the runtime validation must survive the dry-run relocation).\n\
         stdout=\n{stdout}\nstderr=\n{stderr}"
    );
}
