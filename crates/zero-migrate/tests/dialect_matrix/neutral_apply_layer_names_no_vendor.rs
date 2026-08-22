//! Where a vendor's apply code may live, enforced rather than documented.
//!
//! `src/apply/` has two storeys and only one of them is allowed a vendor. The
//! files sitting DIRECTLY under `src/apply/` are the dialect-neutral apply layer
//! — the executor, the drift and baseline flows, the plan-precondition hoist —
//! and `src/apply/backend/mod.rs` is the neutral CONTRACT those flows call
//! through. Underneath it, `src/apply/backend/<dialect>/` is where a vendor is
//! supposed to be, and where all three vendors' session/journal/DML leaves
//! already are.
//!
//! The rule is invisible to every behaviour test. A PostgreSQL-only evaluator
//! parked in the neutral layer still evaluates PostgreSQL preconditions
//! correctly. What it costs is the crate extraction of
//! `docs/proposals/pluggable-backends.md`, and no assertion about behaviour can
//! see that. So the rule gets its own check or it has none.
//!
//! # What this caught when it was written
//!
//! It was RED on the tree that introduced it, which is the only reason to
//! believe it can see anything. It caught `apply/precondition.rs`: 685
//! production lines of `pg_query`, `information_schema` and `&Client` whose own
//! header called it "the POSTGRES precondition impl", sitting in the neutral
//! layer and naming `PostgresBackend` three times in code. It moved to
//! `apply/backend/postgres/precondition.rs`, which is the only place it could
//! go — it needs `SqlSession`/`ExecutorConfig`/`ApplyError`/`Migration`, so it
//! cannot follow the renderers out into `zero-migrate-postgres`, which must not
//! depend on the engine.
//!
//! # What this does NOT catch
//!
//! A file in the neutral layer that is vendor-specific WITHOUT naming a vendor
//! backend type — PostgreSQL-only SQL reached through a neutral helper, say —
//! is invisible here, the same blindness `backend_modules_name_one_dialect.rs`
//! documents at length for its own check. A green run means "no neutral apply
//! file NAMES a vendor backend". It does not mean the neutral layer is free of
//! vendor logic.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// The three backend types. Naming one of these is the cheapest RELIABLE signal
/// that a file is a vendor's implementation rather than a neutral flow: the
/// neutral flows reach a backend only through `dyn MigrationBackend`, so they
/// have no reason to spell a concrete one, and a vendor impl almost always
/// constructs or bounds itself by its own.
const VENDOR_BACKENDS: &[&str] = &["PostgresBackend", "MysqlBackend", "SqliteBackend"];

/// Files the neutral-layer listing MUST contain.
///
/// A scan over a DISCOVERED set fails OPEN: point it at the wrong directory, or
/// let the read silently return nothing, and it passes with zero files
/// inspected. The floor below bounds HOW MANY were found; these bound WHICH, and
/// they stay true however far the layer shrinks. `executor.rs` is the neutral
/// apply flow itself and `mod.rs` is the layer's own root — neither can leave
/// `src/apply/` while there is a neutral apply layer at all.
const NEUTRAL_LAYER_ANCHORS: &[&str] = &["executor.rs", "mod.rs"];

/// The weaker of the two anti-blindness checks; see [`NEUTRAL_LAYER_ANCHORS`]
/// for the one that holds.
///
/// Deliberately low, and its premise is stated so the failure reads as a
/// question. This layer is EXPECTED to shrink as vendor code moves down into
/// `backend/<dialect>/`, so a count that falls is the project working. Lower it
/// when an extraction legitimately moved a file out, and name the extraction in
/// the commit; never lower it to silence a listing that broke, which is what the
/// anchors are for.
const NEUTRAL_LAYER_FLOOR: usize = 4;

/// Whether a source line is CODE rather than prose.
///
/// Line-oriented, and over-counts prose into code rather than the reverse: a
/// vendor name in a trailing `// …` on a code line still counts. That is the
/// safe direction for a check that is trying to prove an ABSENCE.
fn is_code(line: &str) -> bool {
    let t = line.trim_start();
    !(t.starts_with("//") || t.starts_with('*') || t.starts_with("/*"))
}

/// The `.rs` files directly under `src/apply/`, NOT recursing — the whole point
/// is to separate that storey from `backend/<dialect>/` below it.
fn neutral_layer_files() -> Vec<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("apply");
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|entry| entry.expect("dir entry").path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "rs"))
        .collect();
    out.sort();
    out
}

/// A vendor implementation lives under `apply/backend/<dialect>/`, not in the
/// neutral apply layer above it.
#[test]
fn the_neutral_apply_layer_names_no_vendor_backend() {
    let files = neutral_layer_files();
    let names: BTreeSet<String> = files
        .iter()
        .map(|p| {
            p.file_name()
                .expect("a file")
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    // The anchors run FIRST because they answer "did the listing see the layer at
    // all", which is the failure the floor is too blunt to catch.
    for anchor in NEUTRAL_LAYER_ANCHORS {
        assert!(
            names.contains(*anchor),
            "the listing found {names:?} under src/apply/ but never `{anchor}`, so it is \
             not seeing the layer it claims to check. Fix the listing. Do NOT delete the \
             anchor to get green — if `{anchor}` legitimately moved, anchor on another \
             file that cannot leave the neutral layer, and say which in the commit."
        );
    }
    assert!(
        files.len() >= NEUTRAL_LAYER_FLOOR,
        "the listing found only {} files directly under src/apply/, below the floor of \
         {NEUTRAL_LAYER_FLOOR}. The anchors above PASSED, so this is most likely the \
         layer legitimately shrinking as vendor code moves into backend/<dialect>/ — \
         lower the floor and name the extraction that did it.",
        files.len()
    );

    for path in &files {
        let rel = format!(
            "apply/{}",
            path.file_name().expect("a file").to_string_lossy()
        );
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        for backend in VENDOR_BACKENDS {
            let hits = text
                .lines()
                .filter(|l| is_code(l))
                .filter(|l| l.contains(backend))
                .count();
            assert_eq!(
                hits, 0,
                "{rel} names `{backend}` on {hits} code line(s), but it sits in the \
                 DIALECT-NEUTRAL apply layer, which reaches a backend only through \
                 `dyn MigrationBackend`. A file that needs a concrete backend is that \
                 vendor's implementation and belongs under apply/backend/<dialect>/ \
                 with the rest of it."
            );
        }
    }
}
