//! The REPLACEMENT for sixteen `pub(crate)` invariants that a crate boundary
//! dissolved, and the reason a move had to bring a test with it.
//!
//! # What was lost, exactly
//!
//! The schema-shape snapshot value types (`ColumnSnapshot`, `IndexSnapshot`,
//! `ConstraintSnapshot`, `TableSnapshot`, …) moved from `zero-migrate`'s
//! `model/snapshot.rs` into `zero-migrate-backend`, because the `DdlEmitter` trait
//! is moving to that crate and a trait cannot move without the types in its
//! signatures.
//!
//! Sixteen items in that file were `pub(crate)` — visible to the engine, invisible
//! to everything else. `pub(crate)` DOES NOT SURVIVE A CRATE BOUNDARY: the moved
//! file's `crate` is now `zero-migrate-backend`, so `pub(crate)` there would have
//! hidden them from their only callers in the engine. They are `pub` now, which
//! means the compiler no longer refuses what it used to refuse, and the strongest
//! invariant in the file silently became the weakest — precisely because it had
//! never also been a test.
//!
//! | item | home before | what it decides |
//! |------|-------------|-----------------|
//! | `canonical_id_default_expression` | `model/snapshot.rs` | ID-default normal form |
//! | `index_elements_canonically_eq` | `model/snapshot.rs` | index drift comparison |
//! | `canonical_index_sort_order` | `model/snapshot.rs` | index drift comparison |
//! | `index_predicates_canonically_eq` | `model/snapshot.rs` | index drift comparison |
//! | `same_definition_except_name` | `model/snapshot.rs` | constraint rename detection |
//! | `definition_differences_except_name` | `model/snapshot.rs` | constraint drift blame |
//! | `from_sequence_col_type` / `from_pg_type_name` | `model/snapshot.rs` | sequence typing |
//! | `normalize_sequence_min_value` / `_max_value` | `model/snapshot.rs` | sequence bounds |
//! | `sequence_default_start_value` | `model/snapshot.rs` | sequence bounds |
//! | `from_create` / `from_drop` | `model/snapshot.rs` | function-snapshot folding |
//! | `canonical_pg_arg_type` | `model/validate.rs` | PG signature-collision fold |
//!
//! Every one of them COMPARES or NORMALIZES. By this workspace's own boundary rule
//! — stated at length in `zero_migrate::render::backends`'s header — comparison and
//! normalization stay in the engine, dialect-parameterized; only SPELLING is the
//! engine asking a vendor a question. So the property the `pub(crate)` was carrying
//! is still exactly right, and it is this:
//!
//! **no vendor crate calls any of them.**
//!
//! A vendor that reached one would be a vendor deciding whether two schemas differ,
//! which is the engine's judgement and not its own. That is what this file measures,
//! and it is now the only thing that measures it.
//!
//! # Why a census and not a visibility modifier
//!
//! There is no modifier that expresses "visible to `zero-migrate` but not to
//! `zero-migrate-postgres`". `pub(crate)` is too narrow (it would hide them from the
//! engine, their only caller) and `pub` is too wide. The type system genuinely
//! cannot say this, which is why it has to be said here instead of being asserted in
//! prose and forgotten.
//!
//! # The floors, because a scan over a DISCOVERED set fails OPEN
//!
//! Two ways to be green for the wrong reason, and they are different blindnesses:
//! narrow the walk and it iterates nothing; break the needle and it reads every file
//! and matches nothing. So there are two floors — [`VENDOR_FILE_FLOOR`] for the
//! walk, and a POSITIVE CONTROL that runs the identical matcher over the engine's
//! own sources, where these names must still appear. If the engine count goes to
//! zero the matcher has gone blind and the vendor zeroes below it mean nothing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The sixteen degraded items, by the ident a caller would have to write.
///
/// `ColumnSnapshot::new` and `IndexSnapshot::new` were `pub(crate)` too and are NOT
/// here: `new` is far too generic to match on a line-oriented census, and a needle
/// that fires on every constructor in three vendor crates is a needle nobody can
/// keep green. They are the two of the eighteen this file does not cover, and saying
/// so is better than a match that would have to be special-cased into uselessness.
const CORE_ONLY_ITEMS: &[&str] = &[
    "canonical_id_default_expression",
    "index_elements_canonically_eq",
    "canonical_index_sort_order",
    "index_predicates_canonically_eq",
    "same_definition_except_name",
    "definition_differences_except_name",
    "from_sequence_col_type",
    "from_pg_type_name",
    "normalize_sequence_min_value",
    "normalize_sequence_max_value",
    "sequence_default_start_value",
    "sequence_default_min_value",
    "sequence_default_max_value",
    "canonical_pg_signature_type",
    "canonical_pg_arg_type",
    "normalize_sequence_bound",
];

/// The vendor crates, relative to the workspace `crates/` directory. A vendor crate
/// may name a snapshot TYPE — that is what the `DdlEmitter` signatures are made of —
/// but may not call one of the engine's comparison helpers on it.
const VENDOR_CRATES: &[&str] = &[
    "zero-migrate-postgres",
    "zero-migrate-sqlite",
    "zero-migrate-mysql",
];

/// The walk's floor across all three vendor crates. They hold 17 `.rs` files under
/// `src` after the DDL move (6 + 5 + 6). The floor sits under that with room for
/// churn but nowhere near zero, so a walk that lost its root cannot pass.
///
/// Raise it deliberately as the vendors grow. NEVER lower it to get green: a
/// drop means the walk stopped seeing files, which is the failure this defends.
const VENDOR_FILE_FLOOR: usize = 13;

/// The positive control's floor: how many engine source lines must still call one of
/// these. The engine had well over this when the move landed; the number only has to
/// prove the matcher is not blind, so it is set low and blunt on purpose.
const ENGINE_CALLSITE_FLOOR: usize = 10;

/// Whether a source line is CODE rather than a comment.
///
/// Line-oriented on purpose, and the same filter its sibling census
/// `core_names_no_vendor_crate.rs` uses, with the same stated limits: it cannot see
/// inside a block comment that starts mid-line and does not try. It over-counts
/// prose into code, never the reverse, which is the safe direction for a census that
/// is asserting a ZERO.
fn is_code(line: &str) -> bool {
    let t = line.trim_start();
    !(t.starts_with("//") || t.starts_with('*') || t.starts_with("/*"))
}

/// Every `.rs` file under `root`, sorted.
fn src_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Code lines in `files` that name one of [`CORE_ONLY_ITEMS`], keyed by display path.
fn callsites(files: &[PathBuf], base: &Path) -> BTreeMap<String, usize> {
    let mut found = BTreeMap::new();
    for path in files {
        let rel = path
            .strip_prefix(base)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let n = text
            .lines()
            .filter(|l| is_code(l))
            .filter(|l| CORE_ONLY_ITEMS.iter().any(|i| l.contains(i)))
            .count();
        if n > 0 {
            found.insert(rel, n);
        }
    }
    found
}

/// The workspace `crates/` directory.
fn crates_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/zero-migrate has a parent")
        .to_path_buf()
}

#[test]
fn no_vendor_crate_calls_an_engine_snapshot_comparison() {
    let crates = crates_dir();

    // FLOOR ONE — the WALK.
    let mut vendor_files = Vec::new();
    for vendor in VENDOR_CRATES {
        let src = crates.join(vendor).join("src");
        assert!(
            src.is_dir(),
            "vendor source root {} does not exist; the census would walk nothing and \
             report clean",
            src.display()
        );
        vendor_files.extend(src_files(&src));
    }
    assert!(
        vendor_files.len() >= VENDOR_FILE_FLOOR,
        "the census walked only {} vendor files, below the floor of {VENDOR_FILE_FLOOR}. \
         A narrowed walk finds nothing and reports clean; fix the walk, do not lower \
         the floor to get green.",
        vendor_files.len()
    );

    // FLOOR TWO — the NEEDLE, as a positive control. The identical matcher, over the
    // engine, where these names live and are called. A zero here means the matcher is
    // broken and every vendor zero below it is meaningless.
    let engine_src = crates.join("zero-migrate").join("src");
    let engine = callsites(&src_files(&engine_src), &engine_src);
    let engine_total: usize = engine.values().sum();
    assert!(
        engine_total >= ENGINE_CALLSITE_FLOOR,
        "the positive control found only {engine_total} engine call sites for \
         {} idents, below the floor of {ENGINE_CALLSITE_FLOOR}. Either these helpers \
         genuinely left the engine — in which case retire this file deliberately — or \
         `CORE_ONLY_ITEMS` stopped matching and the census is blind.",
        CORE_ONLY_ITEMS.len()
    );

    // THE PROPERTY.
    let violations = callsites(&vendor_files, &crates);
    assert!(
        violations.is_empty(),
        "a vendor crate calls one of the engine's snapshot comparison/normalization \
         helpers:\n{}\n\nThese were `pub(crate)` in `zero-migrate` before the snapshot \
         types moved to `zero-migrate-backend`; the crate boundary made them `pub` and \
         this census is what stands in for the modifier. A vendor decides how to SPELL \
         something, never whether two schemas DIFFER — see \
         `zero_migrate::render::backends`'s header for the rule.",
        violations
            .iter()
            .map(|(f, n)| format!("  {f}: {n} line(s)"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
