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
//! | `from_sequence_col_type` / `from_catalog_type_name` | `model/snapshot.rs` | sequence typing |
//! | `normalize_sequence_min_value` / `_max_value` | `model/snapshot.rs` | sequence bounds |
//! | `sequence_default_start_value` | `model/snapshot.rs` | sequence bounds |
//! | `from_create` / `from_drop` | `model/snapshot.rs` | function-snapshot folding |
//! | `canonical_arg_type` | `model/validate.rs` | PG signature-collision fold |
//!
//! # The sixteen are two kinds, and the PostgreSQL extraction is what proved it
//!
//! The first version of this file said "every one of them COMPARES or NORMALIZES"
//! and asserted one rule over all sixteen: **no vendor crate calls any of them.** That
//! was true while it was written and it was true for the wrong reason — PostgreSQL's
//! catalog reader was still INSIDE the engine, so the only code that could reach the
//! sequence half was engine code by construction. When `apply/backend/postgres/`
//! became `zero-migrate-postgres/src/backend/`, four call sites in one file crossed
//! the boundary and the single rule went red. Nothing about the code's behaviour
//! changed; what changed is that the rule's unstated premise stopped holding.
//!
//! Reading the four honestly separates the list in two:
//!
//! * **[`VERDICT_ITEMS`] — deciding whether two schemas DIFFER.** Index element
//!   equality, sort-order canonicalization, predicate equality, constraint
//!   rename-vs-change blame, the ID-default normal form, the PG signature fold. A
//!   vendor that called one of these would be answering the engine's question with
//!   its own opinion. **ZERO, across all three vendor crates**, and that is the
//!   property the `pub(crate)` was carrying.
//!
//! * **[`SHARED_NORMAL_FORM_ITEMS`] — producing the CURRENCY the verdict is taken
//!   over.** `SequenceSnapshot`'s bounds are `Option<SafeI64>`, where `None` MEANS
//!   "PostgreSQL's default for this type and increment". Both sides of a drift check
//!   have to reduce to that same `None`: the engine's authored fold
//!   (`render::fold::fold_create_sequence_snapshot`) and the vendor's catalog read
//!   (`zero_migrate_postgres::backend::drift_sql`). A vendor calling these is not
//!   deciding difference — it is refusing to re-derive the normal form, which is the
//!   only way the two sides can agree. A SECOND implementation is the defect, and it
//!   is the defect this half now measures.
//!
//! The verdict rule did not weaken. What was one rule with a hidden premise is two
//! rules with stated ones, and the second is enforced in the direction that can
//! actually go wrong now: not "no vendor calls it" (a vendor MUST), but "no vendor
//! re-implements it, and the one that reads a sequence catalog demonstrably reaches
//! the shared one".
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

/// The degraded items that take the VERDICT: a vendor calling one is a vendor
/// deciding whether two schemas differ. **Zero vendor call sites.**
///
/// `ColumnSnapshot::new` and `IndexSnapshot::new` were `pub(crate)` too and are NOT
/// here: `new` is far too generic to match on a line-oriented census, and a needle
/// that fires on every constructor in three vendor crates is a needle nobody can
/// keep green. They are the two of the eighteen this file does not cover, and saying
/// so is better than a match that would have to be special-cased into uselessness.
const VERDICT_ITEMS: &[&str] = &[
    "canonical_id_default_expression",
    "index_elements_canonically_eq",
    "canonical_index_sort_order",
    "index_predicates_canonically_eq",
    "same_definition_except_name",
    "definition_differences_except_name",
    "canonical_signature_type",
    "canonical_arg_type",
];

/// The degraded items that build the shared NORMAL FORM a verdict is taken over.
///
/// A vendor MAY call these — it must, or its catalog read and the engine's authored
/// fold reduce a sequence's bounds differently and every sequence reports drift. What
/// a vendor may NOT do is define its own, which is what [`defines_own_copy`] looks
/// for.
const SHARED_NORMAL_FORM_ITEMS: &[&str] = &[
    "from_sequence_col_type",
    "from_catalog_type_name",
    "normalize_sequence_min_value",
    "normalize_sequence_max_value",
    "sequence_default_start_value",
    "sequence_default_min_value",
    "sequence_default_max_value",
    "normalize_sequence_bound",
    // The `nextval` default's ONE spelling and ONE parse, added when they came down
    // from `zero_migrate::render::declarative` and `zero_migrate::apply::drift`. They
    // are the same kind as the bounds above and by the same argument: the vendor
    // WRITES `nextval('<seq>'::regclass)` and the dialect-blind differ has to READ it,
    // so a second copy on either side does not report a difference, it manufactures
    // one. They were `pub(crate)` in the engine and are `pub` here, which is the
    // degradation this file exists to stand in for.
    "nextval_default_expr",
    "parse_nextval_sequence_ref",
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

/// The SHARED-NORMAL-FORM half's positive control: how many vendor code lines must
/// reach the one implementation.
///
/// Measured at 8, all in `zero-migrate-postgres/src/backend/drift_sql.rs` — the
/// `SequenceDataTypeSnapshot::from_catalog_type_name` call, the two bound normalizers, the
/// `nextval` render and parse, and the `use` lines that import them. Set below that so
/// ordinary churn does not trip it; a drop to zero is the question, and the answer is
/// either "the needle broke" or "a vendor stopped reusing and started re-deriving".
const SHARED_NORMAL_FORM_REUSE_FLOOR: usize = 5;

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

/// Code lines in `files` that name one of `items`, keyed by display path.
fn callsites(files: &[PathBuf], base: &Path, items: &[&str]) -> BTreeMap<String, usize> {
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
            .filter(|l| items.iter().any(|i| l.contains(i)))
            .count();
        if n > 0 {
            found.insert(rel, n);
        }
    }
    found
}

/// Code lines in `files` that DEFINE one of `items` — a second implementation of a
/// shared normal form, which is the thing a vendor must never grow.
///
/// `fn <name>` bounded on the right, so a CALL (`normalize_sequence_min_value(..)`)
/// does not match and a definition (`pub fn normalize_sequence_min_value(`) does.
fn definitions(files: &[PathBuf], base: &Path, items: &[&str]) -> BTreeMap<String, usize> {
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
            .filter(|l| items.iter().any(|i| l.contains(&format!("fn {i}"))))
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
    let all_items: Vec<&str> = VERDICT_ITEMS
        .iter()
        .chain(SHARED_NORMAL_FORM_ITEMS.iter())
        .copied()
        .collect();
    let engine = callsites(&src_files(&engine_src), &engine_src, &all_items);
    let engine_total: usize = engine.values().sum();
    assert!(
        engine_total >= ENGINE_CALLSITE_FLOOR,
        "the positive control found only {engine_total} engine call sites for \
         {} idents, below the floor of {ENGINE_CALLSITE_FLOOR}. Either these helpers \
         genuinely left the engine — in which case retire this file deliberately — or \
         the item lists stopped matching and the census is blind.",
        all_items.len()
    );

    // THE PROPERTY, HALF ONE: no vendor takes the verdict.
    let violations = callsites(&vendor_files, &crates, VERDICT_ITEMS);
    assert!(
        violations.is_empty(),
        "a vendor crate calls one of the engine's snapshot COMPARISON helpers:\n{}\n\n\
         These were `pub(crate)` in `zero-migrate` before the snapshot types moved to \
         `zero-migrate-backend`; the crate boundary made them `pub` and this census is \
         what stands in for the modifier. A vendor decides how to SPELL something, \
         never whether two schemas DIFFER — see `zero_migrate::render::backends`'s \
         header for the rule.",
        violations
            .iter()
            .map(|(f, n)| format!("  {f}: {n} line(s)"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // THE PROPERTY, HALF TWO: no vendor grows its own copy of the shared normal form.
    let forks = definitions(&vendor_files, &crates, SHARED_NORMAL_FORM_ITEMS);
    assert!(
        forks.is_empty(),
        "a vendor crate DEFINES one of the shared snapshot normal-form helpers:\n{}\n\n\
         There is one implementation and both sides of a drift check must reduce \
         through it — the engine's authored fold and the vendor's catalog read. A \
         second copy does not report a difference, it MANUFACTURES one, silently, the \
         first time the two drift apart.",
        forks
            .iter()
            .map(|(f, n)| format!("  {f}: {n} definition(s)"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // AND ITS POSITIVE CONTROL. The vendor that reads a sequence catalog must
    // demonstrably REACH the shared normal form; a zero here would mean either that
    // it stopped (and re-derived one somewhere this file cannot see) or that
    // `SHARED_NORMAL_FORM_ITEMS` stopped matching, in which case the fork check above
    // is blind. Measured on the tree that introduced this half: four code lines in
    // `zero-migrate-postgres/src/backend/drift_sql.rs`.
    let reuse = callsites(&vendor_files, &crates, SHARED_NORMAL_FORM_ITEMS);
    let reuse_total: usize = reuse.values().sum();
    assert!(
        reuse_total >= SHARED_NORMAL_FORM_REUSE_FLOOR,
        "only {reuse_total} vendor code lines reach the shared snapshot normal form, \
         below the floor of {SHARED_NORMAL_FORM_REUSE_FLOOR}. PostgreSQL is the one \
         shipping vendor with sequences and its catalog read must reduce their bounds \
         through the same helpers the engine's authored fold does. Found:\n{}",
        reuse
            .iter()
            .map(|(f, n)| format!("  {f}: {n} line(s)"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
