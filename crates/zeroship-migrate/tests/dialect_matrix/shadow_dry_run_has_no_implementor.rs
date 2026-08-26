//! `ShadowDryRun` is DECLARED and UNIMPLEMENTED, and that has to stay measured
//! rather than remembered.
//!
//! # What this replaces, and why one test is stronger than the three it replaces
//!
//! Until `shadow()` came off `MigrationBackend`, three separate tests asserted
//! `backend.shadow().is_none()` — one in the PostgreSQL backend's unit tests, one
//! in MySQL's, one in the SQLite engine suite. All three passed, all three always
//! would have, and together they still could not see the case that matters: a
//! FOURTH backend, or a host adapter, quietly acquiring a shadow harness. Each test
//! only knew about its own vendor.
//!
//! The capability is a parameter to
//! [`MigrationEngine::dry_run`](zeroship_migrate::MigrationEngine::dry_run) now, so
//! "which backend declares one" is no longer a question the type system asks. The
//! fact that survives is a fact about the whole workspace: **nothing implements
//! `ShadowDryRun`**, so every dry-run refuses with `DryRunError::ShadowUnsupported`
//! and no caller can be handed a false-success report.
//!
//! # This is not a demand that it stay unimplemented
//!
//! A real harness is wanted — the trait's own docs describe the untrusted/AI-authored
//! DDL preview it exists for. When one arrives this test goes red, and that is the
//! point: writing an `impl ShadowDryRun` is not enough on its own, because the engine
//! will still refuse every dry-run until a caller PASSES it. Going red forces the
//! author to wire the harness into the call path and to say so here, instead of
//! shipping an implementation that silently never runs.
//!
//! # The floors, because a scan over a DISCOVERED set fails OPEN
//!
//! Two ways to be green for the wrong reason. Narrow the walk and it reads nothing;
//! break the needle and it reads everything and matches nothing. So there are two
//! controls: [`RS_FILE_FLOOR`] over the walk, and a POSITIVE CONTROL that the trait's
//! own DECLARATION is found by a sibling needle. If the declaration stops matching,
//! the trait was renamed and the `impl` needle below is looking for a name that no
//! longer exists — every zero it reports would be meaningless.

use std::path::{Path, PathBuf};

/// Every `.rs` file under the workspace's `crates/`, walked. Not a fixed list:
/// the point is to see a vendor crate that does not exist yet.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // `target/` is build output, not source, and it contains vendored
            // dependency sources that would swamp the walk.
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            walk(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The walk's floor. The workspace has eight crates; a walk that reads fewer files
/// than this has been narrowed by a directory rename or a broken `read_dir`, and its
/// zero below would mean nothing. Raise it freely; lowering it needs the reason.
const RS_FILE_FLOOR: usize = 200;

/// A line that is CODE, not prose. The trait is discussed by name in a lot of doc
/// comments — including this file's own header, and the engine methods that explain
/// why the capability is a parameter — and a scan that read prose would be red for a
/// paragraph, which is how a source check gets weakened until it means nothing.
fn is_code(line: &str) -> bool {
    let t = line.trim_start();
    !(t.starts_with("//") || t.starts_with("/*") || t.starts_with('*'))
}

#[test]
fn shadow_dry_run_has_no_implementor() {
    let crates_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/zeroship-migrate has a parent");

    let mut files = Vec::new();
    walk(crates_dir, &mut files);

    assert!(
        files.len() >= RS_FILE_FLOOR,
        "the census walked only {} .rs files under {}, below the floor of \
         {RS_FILE_FLOOR}. The walk has been narrowed — a directory rename, or a \
         `read_dir` that is failing silently — so the zero it reports below is not \
         evidence of anything. Fix the walk before touching the floor.",
        files.len(),
        crates_dir.display(),
    );

    let mut implementors: Vec<String> = Vec::new();
    let mut declarations = 0usize;

    for path in &files {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let rel = path
            .strip_prefix(crates_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        for line in text.lines().filter(|l| is_code(l)) {
            let t = line.trim();
            // The NEEDLE's positive control: the trait declaration itself. Written
            // with the same `ShadowDryRun` token the `impl` needle uses, so a rename
            // takes both to zero at once and the assert below fires.
            if t.starts_with("pub trait ShadowDryRun") || t.starts_with("trait ShadowDryRun") {
                declarations += 1;
            }
            // `impl ShadowDryRun for X`, and the generic/lifetime forms of it.
            if t.starts_with("impl") && t.contains("ShadowDryRun for ") {
                implementors.push(format!("  {rel}: {t}"));
            }
        }
    }

    // FLOOR TWO — the NEEDLE. If the trait's own declaration is not found, the token
    // this test searches for is not the token the code uses, and the empty
    // implementor list below is an artifact of a broken scan rather than a fact.
    assert_eq!(
        declarations, 1,
        "the needle found {declarations} `ShadowDryRun` trait declaration(s), expected \
         exactly 1. Either the trait was renamed or moved — update this control \
         deliberately, in the same commit — or the matcher has gone blind, in which \
         case the implementor scan below proves nothing."
    );

    assert!(
        implementors.is_empty(),
        "`ShadowDryRun` now has an implementor:\n{}\n\
         \n\
         This test is not asking you to remove it. It is asking you to FINISH it. \
         `MigrationEngine::dry_run` / `dry_run_declarative` take the harness as an \
         `Option<&dyn ShadowDryRun>` parameter, so an implementation that is never \
         PASSED to them changes nothing: every dry-run still returns \
         `DryRunError::ShadowUnsupported`, and a caller that believes a dry-run \
         happened when it did not is the one outcome the whole capability exists to \
         prevent.\n\
         \n\
         So: wire it into the call path, then update this test to name the \
         implementor and assert that the wired path returns a real `DryRunReport` \
         rather than the refusal. Deleting the assertion instead would restore \
         exactly the blindness it was written to remove.",
        implementors.join("\n"),
    );
}
