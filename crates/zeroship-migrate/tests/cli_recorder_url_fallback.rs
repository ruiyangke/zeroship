#![cfg(feature = "zsv8")]
//! PR4 code-critic LOW #1 — `build --recorder-url` honesty: the dev CLI embeds NO
//! hosted-recorder HTTP client, so it ALWAYS falls back to the LOCAL recorder. That
//! downgrade must be LOUD (a per-file warning on stderr), not a silent substitution
//! of local for the requested hosted path. Security is preserved by the deploy-time
//! provenance re-record; the warning is purely an honesty fix.
//!
//! This is a FAITHFUL CLI e2e: it runs the REAL `zeroship-migrate-js` binary
//! (`new` then `build --recorder-url`), which drives the REAL kernel-sandboxed
//! recorder child for the local fallback. Per the faithful-e2e rule it HARD-FAILS
//! (asserts the child binary exists) rather than silently skipping a leg.
//!
//! RED proof: before the fix `cmd_build` printed only the `via HostedFellBackToLocal`
//! debug line and NO warning; this test asserts the explicit WARNING string is
//! present on stderr.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Command;

use zeroship_migrate::frontend::recorder_service::recorder_child_path;

fn cli_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zeroship-migrate-js"))
}

#[test]
fn build_with_recorder_url_warns_loudly_on_local_fallback() {
    let child = recorder_child_path();
    assert!(
        child.exists(),
        "recorder child binary not found at {} — build it (cargo build -p zeroship-migrate \
         --bin zeroship-migrate-recorder-child); this faithful e2e must NOT silent-skip",
        child.display()
    );

    let dir = tempfile::tempdir().unwrap();
    let mig_dir = dir.path().join("migrations");

    // 1. Scaffold a deterministic op.* migration via the REAL `new` subcommand.
    let new_out = Command::new(cli_bin())
        .args(["new", "widgets", "--dir"])
        .arg(&mig_dir)
        .output()
        .expect("spawn `new`");
    assert!(
        new_out.status.success(),
        "`new` failed: {}",
        String::from_utf8_lossy(&new_out.stderr)
    );

    // 2. `build --recorder-url <url>`: the dev CLI has no hosted client → it falls
    //    back to the LOCAL recorder and MUST warn loudly.
    let build_out = Command::new(cli_bin())
        .args(["build", "--dir"])
        .arg(&mig_dir)
        .args(["--recorder-url", "https://recorder.example/v1"])
        .output()
        .expect("spawn `build --recorder-url`");
    assert!(
        build_out.status.success(),
        "`build` failed: {}",
        String::from_utf8_lossy(&build_out.stderr)
    );
    let stderr = String::from_utf8_lossy(&build_out.stderr);
    assert!(
        stderr.contains("WARNING") && stderr.contains("recorded LOCALLY"),
        "the local-fallback downgrade must be LOUD on stderr; got: {stderr}"
    );
    assert!(
        stderr.contains("--recorder-url"),
        "the warning must name the requested option; got: {stderr}"
    );

    // The transient build still reports the fallback path.
    let stdout = String::from_utf8_lossy(&build_out.stdout);
    assert!(
        stdout.contains("HostedFellBackToLocal"),
        "the recorded path is the local fallback; got: {stdout}"
    );
}
