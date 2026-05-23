//! Backlog R4-T1: shellcheck regression gate for sandbox shell scripts.
//!
//! Spawns `crates/sandbox/scripts/lint.sh`, which iterates every sibling
//! `*.sh` and runs `shellcheck --severity=error`. The lint script exits
//! 127 ("command not found", POSIX convention) when shellcheck itself
//! isn't on PATH, so this test maps that to `#[ignore]` rather than a
//! spurious failure.
//!
//! Why a separate lint.sh + integration test (not a `cargo` build.rs or
//! a clippy lint):
//!   - portable: developers without shellcheck still get a green
//!     `cargo test -p zeroship-sandbox`,
//!   - CI can flip the gate on by installing shellcheck and dropping
//!     `--ignored`,
//!   - the lint.sh wrapper is independently runnable (e.g. from a
//!     pre-commit hook or a `make lint` recipe), no cargo dependency.
//!
//! See `docs/reviews/sandbox-snapshot-restore-deferred.md` R4-T1 for the
//! provenance — test-coverage-r4 ran shellcheck on `nomad-vm-wrapper.sh`
//! and got 5 INFO-level only, zero errors/warnings, but B12/B13/B17 were
//! all wrapper-bash bugs that a permanent gate would catch pre-cluster.

use std::path::PathBuf;
use std::process::Command;

/// Locate `crates/sandbox/scripts/lint.sh` relative to the manifest dir.
/// `CARGO_MANIFEST_DIR` is set by cargo for both `cargo test` and
/// `cargo build --tests`, so this resolves correctly whether invoked
/// from the workspace root or the crate dir.
fn lint_script() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join("scripts").join("lint.sh")
}

/// Exit code 127 = "command not found"; lint.sh emits this when the
/// shellcheck binary is absent. We mirror that contract here.
const EXIT_SHELLCHECK_MISSING: i32 = 127;

#[test]
fn sandbox_scripts_pass_shellcheck_error_severity() {
    let script = lint_script();
    assert!(
        script.exists(),
        "lint.sh missing at {} — backlog R4-T1 expects it to live next to the scripts it gates",
        script.display(),
    );

    let output = Command::new(&script)
        .output()
        .expect("failed to spawn lint.sh — is bash on PATH?");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let code = output.status.code().unwrap_or(-1);

    if code == EXIT_SHELLCHECK_MISSING {
        // Promote to an explicit skip with a clear next step. The test
        // is also `#[ignore]`-able via the companion test below; this
        // branch handles the case where someone runs without --ignored
        // on a host that still lacks shellcheck (e.g. fresh dev box).
        eprintln!(
            "scripts_lint: shellcheck not installed — skipping gate.\n\
             Install with `apt-get install shellcheck` (Debian/Ubuntu),\n\
             `brew install shellcheck` (macOS), or your distro equivalent.\n\
             lint.sh stderr:\n{stderr}",
        );
        return;
    }

    assert_eq!(
        code, 0,
        "shellcheck --severity=error failed for one or more sandbox scripts.\n\
         stdout:\n{stdout}\n\
         stderr:\n{stderr}",
    );
}

/// Companion test that *requires* shellcheck to be installed. CI flips
/// this on by passing `--ignored`. Local devs without shellcheck still
/// get a green `cargo test -p zeroship-sandbox`, while CI gets a hard
/// gate. The non-ignored test above runs in both modes and skips
/// gracefully when the binary is absent.
#[test]
#[ignore = "requires shellcheck on PATH; run with `cargo test --test scripts_lint -- --ignored`"]
fn sandbox_scripts_pass_shellcheck_error_severity_required() {
    let script = lint_script();
    let output = Command::new(&script)
        .output()
        .expect("failed to spawn lint.sh — is bash on PATH?");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let code = output.status.code().unwrap_or(-1);

    assert_ne!(
        code, EXIT_SHELLCHECK_MISSING,
        "shellcheck is not installed but this test was run with --ignored \
         (which is meant for CI environments where shellcheck IS provisioned). \
         Install shellcheck or drop --ignored.\n\
         stderr:\n{stderr}",
    );
    assert_eq!(
        code, 0,
        "shellcheck --severity=error failed.\n\
         stdout:\n{stdout}\n\
         stderr:\n{stderr}",
    );
}
