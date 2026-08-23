//! The census that replaces a MODULE PRIVACY a crate boundary could not carry.
//!
//! # What was lost, exactly
//!
//! `format_mysql_physical_type` was a bare `fn` in `zero-migrate/src/apply/drift.rs`:
//! a MySQL type-text renderer, one arm per family, module-private, so `render/`,
//! `schema/` and every other part of the engine were PHYSICALLY UNABLE TO NAME IT. That
//! unreachability was the invariant. The engine may COMPARE a
//! [`MysqlPhysicalType`](zero_migrate_backend::snapshot::MysqlPhysicalType) — that is
//! its job, and `apply::drift::column_data_types_eq` does it — but the engine emitting
//! MySQL type text into DDL would be core spelling one vendor's bytes, which is the
//! rule `core_does_not_spell_a_vendors_bytes.rs` states at length.
//!
//! It is `MysqlPhysicalType::type_text` now, `pub`, in `zero-migrate-backend`, beside
//! the `MysqlPhysicalType::parse` it is the exact inverse of. That pairing is the
//! point of the move — one contract, both directions, one file, so a change to either
//! half meets the other — but it costs the privacy: `pub(crate)` in the new home would
//! hide it from its only caller, which is in `zero-migrate`. There is no modifier for
//! "reachable from `apply/drift.rs` and nowhere else in the engine". So it is said
//! here.
//!
//! # The property
//!
//! Two halves, and the second is the one the privacy used to give for free:
//!
//! 1. THE PAIR IS IN ONE PLACE. `type_text` is defined in `snapshot.rs` beside
//!    `parse`, and NOT in `drift.rs`. Two-sided, because "it is in the new home"
//!    alone passes on a leftover copy in the old one, and "it is gone from the old
//!    home" alone passes on a deletion.
//! 2. THE ENGINE REACHES IT FROM ONE FILE. `apply/drift.rs` prints the two sides of a
//!    drift REPORT line; nothing else in `crates/zero-migrate/src` may spell a MySQL
//!    type. A vendor crate calling it is fine and deliberately unconstrained here —
//!    spelling for itself is the only thing a vendor is for.
//!
//! # The floors, because a scan over a DISCOVERED set fails OPEN
//!
//! Narrow the walk and it iterates nothing and reports clean; break the needle and it
//! reads every file and matches nothing. A bare count floor catches neither reliably —
//! this tree has already had a narrowed walk pass a floor — so the walk carries a
//! SUBJECT ANCHOR as well: the one file that must be in the walked set is asserted by
//! path, and its call count is the needle's positive control.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The speller's ident. Chosen because it was MEASURED collision-free across
/// `crates/` first: `git grep type_text` found nothing, so every match this census
/// reports is this function and nothing else. The more idiomatic `spelling` was
/// rejected for the opposite reason - it already appears across the render and fold
/// layers, and a needle that fires on unrelated code is a census nobody can read.
const SPELLER: &str = "type_text";

/// The speller's parser half. Both names are needled at the home so the census fails
/// if the PAIR is broken up, not merely if the speller moves.
const PARSER: &str = "fn parse(";

/// The one physical home, and the file the speller LEFT.
const SPELLER_HOME: &str = "crates/zero-migrate-backend/src/snapshot.rs";
const FORMER_SPELLER_HOME: &str = "crates/zero-migrate/src/apply/drift.rs";

/// The ident of the renderer that used to live in the former home. Its ABSENCE there
/// is half the anchor; a leftover copy would be a second physical home for type text
/// whose whole discipline is having exactly one.
const FORMER_SPELLER: &str = "format_mysql_physical_type";

/// The engine file that MAY spell a MySQL type, workspace-relative.
///
/// `column_data_type_report` prints the two sides of a `data_type` drift line. Both
/// sides carry a contract there — that is the condition it checks first — so it is
/// holding a `MysqlPhysicalType` and asking that value for its own text, not resolving
/// a vendor to answer a question. The report is prose for an operator, never DDL.
const ENGINE_FILE_THAT_MAY_SPELL: &str = "apply/drift.rs";

/// The walk's floor over `crates/zero-migrate/src`. Set under the engine's file count
/// with room for churn but nowhere near zero; raise it deliberately as the engine
/// grows, and never lower it to get green.
///
/// A floor ALONE is not enough and this one is not trusted alone: a narrowed walk in
/// this tree has already come in above its floor. [`ENGINE_FILE_THAT_MAY_SPELL`] is
/// asserted to be IN the walked set, by path, which is the check a count cannot make.
const ENGINE_FILE_FLOOR: usize = 70;

/// The needle's positive control: call sites the allowed file is known to make.
///
/// `column_data_type_report` calls the speller twice, once per side. If this goes to
/// zero the needle has stopped matching and the zeroes everywhere else mean nothing.
/// A drop is a QUESTION — either the needle broke (fix the needle) or the engine
/// stopped printing contracts (lower this deliberately, in the commit that does it).
const ALLOWED_CALLSITE_FLOOR: usize = 2;

/// Whether a source line is CODE rather than a comment. The same line-oriented filter
/// its sibling censuses use, with the same stated limit: it cannot see inside a block
/// comment that opens mid-line. It over-counts prose as code, never the reverse, which
/// is the safe direction for a census asserting a ZERO.
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

/// Code lines naming [`SPELLER`], keyed by path relative to `base`.
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
            .filter(|l| l.contains(SPELLER))
            .count();
        if n > 0 {
            found.insert(rel, n);
        }
    }
    found
}

#[test]
fn mysql_type_text_lives_with_its_parser() {
    // `include_str!` is a compile-time dependency: editing either file rebuilds this
    // binary, so the anchor cannot drift out from under an edit, and a dangling path
    // is a build error rather than a quiet pass.
    let home = include_str!("../../../zero-migrate-backend/src/snapshot.rs");
    let former = include_str!("../../src/apply/drift.rs");

    // ANCHOR, side one: the pair is together in the new home.
    assert!(
        home.contains(SPELLER),
        "{SPELLER_HOME} does not define `{SPELLER}`. The MySQL speller is supposed to \
         live beside the parser it inverts; if it moved again, move this anchor with it."
    );
    assert!(
        home.contains(PARSER),
        "{SPELLER_HOME} no longer defines `{PARSER}`. The whole reason `{SPELLER}` is \
         in that file is to sit beside its inverse; if the parser left, the pairing \
         this census defends is already broken."
    );

    // ANCHOR, side two: the old home kept no copy.
    assert!(
        !former.contains(FORMER_SPELLER),
        "{FORMER_SPELLER_HOME} still names `{FORMER_SPELLER}`. That renderer moved to \
         `MysqlPhysicalType::{SPELLER}`; a copy left behind is a second physical home \
         for MySQL type text, and the two will drift."
    );

    let engine_src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = src_files(&engine_src);

    // FLOOR ONE, the WALK — plus the SUBJECT ANCHOR a count cannot replace.
    assert!(
        files.len() >= ENGINE_FILE_FLOOR,
        "the census walked only {} files under {}, below the floor of \
         {ENGINE_FILE_FLOOR}. A narrowed walk finds nothing and reports clean; fix the \
         walk, never lower the floor to get green.",
        files.len(),
        engine_src.display()
    );
    let walked: Vec<String> = files
        .iter()
        .map(|p| {
            p.strip_prefix(&engine_src)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    assert!(
        walked.iter().any(|p| p == ENGINE_FILE_THAT_MAY_SPELL),
        "the walk did not reach {ENGINE_FILE_THAT_MAY_SPELL}, the one file this census \
         is actually about. A walk above its floor can still miss its subject - that \
         has happened in this tree - so the subject is named rather than counted."
    );

    // FLOOR TWO, the NEEDLE, as a positive control over the allowed file.
    let found = callsites(&files, &engine_src);
    let allowed = found.get(ENGINE_FILE_THAT_MAY_SPELL).copied().unwrap_or(0);
    assert!(
        allowed >= ALLOWED_CALLSITE_FLOOR,
        "the positive control found {allowed} `{SPELLER}` call site(s) in \
         {ENGINE_FILE_THAT_MAY_SPELL}, below the floor of {ALLOWED_CALLSITE_FLOOR}. \
         Either the needle stopped matching - in which case every zero below is \
         meaningless - or the engine stopped printing the contract, which is a \
         deliberate change this floor should be lowered for in the same commit."
    );

    // THE PROPERTY.
    let strays: Vec<_> = found
        .iter()
        .filter(|(f, _)| f.as_str() != ENGINE_FILE_THAT_MAY_SPELL)
        .map(|(f, n)| format!("  {f}: {n} line(s)"))
        .collect();
    assert!(
        strays.is_empty(),
        "the engine spells a MySQL type outside {ENGINE_FILE_THAT_MAY_SPELL}:\n{}\n\n\
         `format_mysql_physical_type` was module-private to `apply::drift` and the \
         compiler enforced this; across a crate boundary `MysqlPhysicalType::{SPELLER}` \
         has to be `pub`, and this census is what stands in for the modifier. Core \
         COMPARES a physical contract - that is its judgement to make - but the text a \
         vendor's server would write is the vendor's answer, not core's. See \
         `core_does_not_spell_a_vendors_bytes.rs` for the rule and \
         `zero_migrate::render::backends`'s header for why.",
        strays.join("\n")
    );
}
