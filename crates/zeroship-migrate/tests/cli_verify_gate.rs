//! PR4 code-critic MED-2 — the CI gate is RUNNABLE: the `verify` subcommand wires
//! `recheck_not_yet_applied` + `assert_packed_hash_matches_committed` into a real
//! binary entry point that exits NON-ZERO on divergence. Before this fix both gate
//! functions were exported + unit-tested but invoked by NO non-test caller, so the
//! §8.9.1 "CI regenerates each not-yet-applied .ir.json and asserts the typed-value
//! checksum matches the committed blob" deliverable was not actually runnable.
//!
//! FAITHFUL CLI e2e: drives the REAL `zeroship-migrate-js` binary (`build` then
//! `verify`), which drives the REAL kernel-sandboxed recorder child for the re-record
//! gate. Per the faithful-e2e rule it HARD-FAILS (asserts the child binary exists)
//! rather than silently skipping a leg.
//!
//! RED/GREEN: `verify` exits 0 on a clean committed set, and NON-ZERO on a tampered
//! `.ir.json` (checksum / packed-hash divergence).

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Command;

use zeroship_migrate::frontend::recorder_service::recorder_child_path;

fn cli_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zeroship-migrate-js"))
}

fn assert_child_built() {
    let child = recorder_child_path();
    assert!(
        child.exists(),
        "recorder child binary not found at {} — build it (cargo build -p zeroship-migrate \
         --bins); this faithful e2e must NOT silent-skip",
        child.display()
    );
}

/// Build a migrations dir with one recorded migration via the REAL binary.
fn build_one(mig_dir: &std::path::Path) {
    // Scaffold a deterministic op.* migration.
    let new_out = Command::new(cli_bin())
        .args(["new", "gizmos", "--dir"])
        .arg(mig_dir)
        .output()
        .expect("spawn `new`");
    assert!(
        new_out.status.success(),
        "`new` failed: {}",
        String::from_utf8_lossy(&new_out.stderr)
    );
    // Record + commit the `.ir.json`.
    let build_out = Command::new(cli_bin())
        .args(["build", "--dir"])
        .arg(mig_dir)
        .output()
        .expect("spawn `build`");
    assert!(
        build_out.status.success(),
        "`build` failed: {}",
        String::from_utf8_lossy(&build_out.stderr)
    );
}

fn committed_ir_path(mig_dir: &std::path::Path) -> PathBuf {
    std::fs::read_dir(mig_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().ends_with(".ir.json"))
        .expect("a committed .ir.json must exist after build")
}

#[test]
fn verify_exits_zero_on_a_clean_committed_set() {
    assert_child_built();
    let dir = tempfile::tempdir().unwrap();
    let mig_dir = dir.path().join("migrations");
    build_one(&mig_dir);

    let out = Command::new(cli_bin())
        .args(["verify", "--dir"])
        .arg(&mig_dir)
        .output()
        .expect("spawn `verify`");
    assert!(
        out.status.success(),
        "`verify` must exit ZERO on a clean committed set; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("verify: OK"), "got: {stdout}");
}

#[test]
fn verify_exits_nonzero_on_a_tampered_ir_json() {
    assert_child_built();
    let dir = tempfile::tempdir().unwrap();
    let mig_dir = dir.path().join("migrations");
    build_one(&mig_dir);

    // TAMPER the committed `.ir.json` ops so its typed-value checksum diverges from
    // what the `.ts` re-records (rename the created table — a real, parseable ops
    // change).
    let ir_path = committed_ir_path(&mig_dir);
    let committed = std::fs::read_to_string(&ir_path).unwrap();
    let tampered = committed.replace("\"gizmos\"", "\"tampered_gizmos\"");
    assert_ne!(committed, tampered, "the tamper must change the committed ops");
    std::fs::write(&ir_path, tampered.as_bytes()).unwrap();

    let out = Command::new(cli_bin())
        .args(["verify", "--dir"])
        .arg(&mig_dir)
        .output()
        .expect("spawn `verify`");
    assert!(
        !out.status.success(),
        "`verify` must exit NON-ZERO on a tampered .ir.json; stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("verify:") && (stderr.contains("checksum") || stderr.contains("diverges")),
        "the failure must name the checksum divergence; got: {stderr}"
    );
}

#[test]
fn verify_skips_applied_versions() {
    assert_child_built();
    let dir = tempfile::tempdir().unwrap();
    let mig_dir = dir.path().join("migrations");
    build_one(&mig_dir);

    // Tamper, but mark the version as ALREADY APPLIED → the re-record gate skips it.
    let ir_path = committed_ir_path(&mig_dir);
    let committed = std::fs::read_to_string(&ir_path).unwrap();
    std::fs::write(
        &ir_path,
        committed.replace("\"gizmos\"", "\"tampered\"").as_bytes(),
    )
    .unwrap();

    // Derive the 14-digit version prefix from the committed file stem.
    let stem = ir_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .strip_suffix(".ir.json")
        .unwrap()
        .to_string();
    let version = &stem[..14];

    let out = Command::new(cli_bin())
        .args(["verify", "--dir"])
        .arg(&mig_dir)
        .args(["--applied", version])
        .output()
        .expect("spawn `verify --applied`");
    // NOTE: an APPLIED migration is frozen, so the re-record gate skips it. The
    // packed-hash gate still runs but only asserts entry-hash tracks disk (which the
    // packer copy guarantees), so it passes on the tampered-but-copied bytes.
    assert!(
        out.status.success(),
        "`verify --applied <v>` must skip the applied (tampered) migration; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
