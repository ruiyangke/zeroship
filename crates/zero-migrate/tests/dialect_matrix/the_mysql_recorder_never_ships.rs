//! The REPLACEMENT for the `#[cfg(test)]` that used to hide MySQL's canned
//! `SqlSession`, and the one thing a `cargo test` run cannot check about itself.
//!
//! # What was lost, exactly
//!
//! `RecordingSession` and its canned `information_schema` rows were a private
//! `#[cfg(test)] mod render_tests` inside `zero-migrate`'s
//! `apply/backend/mysql/mod.rs`. Nothing outside that module could name them, and
//! nothing outside a test build compiled them.
//!
//! When the MySQL execution half moved into `zero-migrate-mysql`, fourteen of its
//! sixty-nine tests could not follow: they drive `apply_with_lock_backend`,
//! `MigrationEngine`, `diff_snapshots` and `fold_ops`, which are the engine's, and
//! `zero-migrate` depends on `zero-migrate-mysql`, so the edge back is a cycle Cargo
//! refuses. They live in `zero-migrate/tests/mysql_engine/` now — and they drive the
//! SAME recorder, because a second copy of a canned catalog is the hazard: its rows
//! are the shared premise of both suites, and two copies drift silently until one
//! suite is asserting against a table shape the other already corrected.
//!
//! So the recorder is `pub`, behind a `testing` feature. `#[cfg(test)]` became
//! `#[cfg(any(test, feature = "testing"))]`, which is strictly weaker: a `cfg(test)`
//! module cannot be compiled into a shipping artifact at all, and a feature CAN be —
//! by anyone who writes one line.
//!
//! # The rule
//!
//! **`testing` is enabled by DEV-dependency edges only.**
//!
//! Resolver 3 is what makes that mean something: it does not unify dev-dependency
//! features into the normal build, so `cargo build -p zero-migrate` compiles a
//! `zero-migrate-mysql` with the feature OFF and the recorder absent. One
//! `[dependencies]` edge that turned it on would silently undo that for every
//! downstream consumer, including the published `.node` addon.
//!
//! # Why a census and not a compiler check
//!
//! Because the compiler is on the wrong side of the question. Every check that could
//! run here runs inside `cargo test`, where the feature is ON by design — a
//! `cfg!(feature = "testing")` assertion in this file would be measuring the test
//! build's own configuration and would be true no matter how the feature got enabled.
//! The manifests are the only artifact that states the intent, so they are what this
//! reads.
//!
//! # The floors, because a scan over a DISCOVERED set fails OPEN
//!
//! A manifest walk that finds nothing reports clean, and a needle that never matches
//! reports clean. So:
//!
//! 1. [`MANIFEST_ANCHORS`] — the two manifests that must be found and parsed. Naming
//!    them bounds WHICH were read, which a count cannot do.
//! 2. The `testing` edge itself is a positive control: exactly one enabling edge must
//!    EXIST. Zero would mean the feature is dead — or, far more likely, that the
//!    matcher stopped recognizing the line it is supposed to police.

use std::path::{Path, PathBuf};

/// The gate the recorder module must carry, verbatim.
///
/// `test` for this crate's own unit tests, `feature = "testing"` for the engine's.
/// A bare `#[cfg(feature = "testing")]` would break the vendor's own suite; a bare
/// `#[cfg(test)]` would break the engine's. Both halves are load-bearing, so the
/// string is asserted rather than described.
const RECORDER_GATE: &str = r#"#[cfg(any(test, feature = "testing"))]
pub mod recording;"#;

/// The vendor manifest that DECLARES the feature, relative to `crates/`.
const VENDOR_MANIFEST: &str = "zero-migrate-mysql/Cargo.toml";

/// The engine manifest that ENABLES it, relative to `crates/`.
const ENGINE_MANIFEST: &str = "zero-migrate/Cargo.toml";

/// The recorder's home, relative to `crates/`.
const RECORDER_HOME: &str = "zero-migrate-mysql/src/backend/mod.rs";

/// Files this census must read. The real defence against a walk that found nothing:
/// they bound WHICH files were inspected, and none of the three can move without
/// this census needing to be reread anyway.
const MANIFEST_ANCHORS: &[&str] = &[VENDOR_MANIFEST, ENGINE_MANIFEST, RECORDER_HOME];

fn crates_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/zero-migrate has a parent")
        .to_path_buf()
}

/// Every `Cargo.toml` under `crates/`, sorted.
fn manifests(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                stack.push(path);
            } else if path.file_name().is_some_and(|n| n == "Cargo.toml") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// The `[section]` a manifest line sits under, and the line itself, for every CODE
/// line naming `zero-migrate-mysql` with the `testing` feature.
fn testing_edges(text: &str) -> Vec<(String, String)> {
    let mut section = String::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = trimmed.to_string();
            continue;
        }
        if trimmed.starts_with("zero-migrate-mysql") && trimmed.contains("\"testing\"") {
            out.push((section.clone(), trimmed.to_string()));
        }
    }
    out
}

/// The recorder is a TEST double: only a dev-dependency edge may turn it on.
#[test]
fn only_a_dev_dependency_enables_the_mysql_recorder() {
    let crates = crates_root();

    // ---- FLOOR ONE: the WALK, bounded by WHICH files. ------------------------
    for anchor in MANIFEST_ANCHORS {
        let path = crates.join(anchor);
        assert!(
            path.is_file(),
            "this census must read {}, and it does not exist. Fix the path. Do NOT \
             delete the anchor to get green — if the file legitimately moved, repoint \
             the anchor and say so in the commit.",
            path.display()
        );
    }

    // The gate itself, verbatim. Both halves of `any(test, feature = "testing")` are
    // load-bearing; either alone breaks one of the two suites that share the double.
    let home = std::fs::read_to_string(crates.join(RECORDER_HOME)).expect("recorder home reads");
    assert!(
        home.contains(RECORDER_GATE),
        "{RECORDER_HOME} no longer gates the recorder on `any(test, feature = \
         \"testing\")`. That gate is what keeps a TEST double out of a shipping \
         build while still letting the engine's integration tests drive the same \
         object. Expected verbatim:\n{RECORDER_GATE}"
    );

    // The feature must still be DECLARED by the vendor: a `[features]` entry that
    // vanished would make every `features = ["testing"]` edge below a hard error,
    // but a RENAMED one would silently leave this census matching nothing.
    let vendor = std::fs::read_to_string(crates.join(VENDOR_MANIFEST)).expect("vendor manifest");
    assert!(
        vendor.lines().any(|l| l.trim() == "testing = []"),
        "{VENDOR_MANIFEST} no longer declares the `testing` feature, so the needle \
         below is matching a name nothing uses and its zero means nothing."
    );

    // ---- FLOOR TWO + THE PROPERTY. -----------------------------------------
    //
    // Every enabling edge in the workspace, with the section it sits under. The
    // count is the positive control (zero means the matcher went blind, not that the
    // tree got safer) and the section is the property.
    let mut edges: Vec<(String, String, String)> = Vec::new();
    for manifest in manifests(&crates) {
        let rel = manifest
            .strip_prefix(&crates)
            .unwrap_or(&manifest)
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(&manifest)
            .unwrap_or_else(|e| panic!("reading {}: {e}", manifest.display()));
        for (section, line) in testing_edges(&text) {
            edges.push((rel.clone(), section, line));
        }
    }

    assert!(
        !edges.is_empty(),
        "no manifest under crates/ enables `zero-migrate-mysql`'s `testing` feature. \
         Either the engine's integration tests lost their recorder — in which case \
         `tests/mysql_engine/mysql_recorded_engine_paths.rs` cannot be compiling — or \
         this census stopped recognizing the line it polices, and its verdict below \
         is worthless."
    );

    let shipping: Vec<String> = edges
        .iter()
        .filter(|(_, section, _)| section != "[dev-dependencies]")
        .map(|(rel, section, line)| format!("  {rel} {section}: {line}"))
        .collect();
    assert!(
        shipping.is_empty(),
        "a NON-dev dependency edge enables the MySQL recorder:\n{}\n\n`testing` \
         publishes `zero_migrate_mysql::backend::recording`, a canned `SqlSession` \
         test double. It replaced a `#[cfg(test)]` module, which could not reach a \
         shipping artifact at all; a feature can, and resolver 3 keeps it out of the \
         normal build ONLY because every edge that turns it on is a dev edge. One \
         `[dependencies]` line here puts a test double inside the published library \
         and inside the `.node` addon built from it.",
        shipping.join("\n")
    );
}
