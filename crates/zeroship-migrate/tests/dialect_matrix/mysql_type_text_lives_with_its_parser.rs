//! The census that replaces a MODULE PRIVACY a crate boundary could not carry.
//!
//! # What was lost, exactly
//!
//! `format_mysql_physical_type` was a bare `fn` in `zeroship-migrate-core/src/apply/drift.rs`:
//! a MySQL type-text renderer, one arm per family, module-private, so `render/`,
//! `schema/` and every other part of the engine were PHYSICALLY UNABLE TO NAME IT.
//! That unreachability was the invariant, and it survived one move only partly. When
//! `MysqlPhysicalType` was a field on the neutral `ColumnSnapshot`, the engine still
//! COMPARED one and printed one, so this census allowed `apply/drift.rs` exactly two
//! call sites and forbade the rest.
//!
//! # What changed, and why the property is now stronger
//!
//! The type left the neutral crate. It is `zeroship_migrate_mysql::physical_type::
//! MysqlPhysicalType` now, and it reaches a `ColumnSnapshot` as an opaque leg of a
//! `Dialectal` carrier keyed by dialect id. The engine still gets the two sides of a
//! `data_type` drift line, but it gets them ALREADY WRITTEN: it asks the leg via
//! `VendorColumnFacts::type_drift_report` and the vendor spells them.
//!
//! So the allowance is gone. The engine may no longer spell a MySQL type ANYWHERE,
//! and this file asserts a flat zero over `crates/zeroship-migrate-core/src` rather than a
//! zero-outside-one-file. Core may not even name the type: `core_names_no_vendor_
//! crate.rs` is the ratchet that says so, and this census is the narrower statement
//! about the SPELLING specifically, which would survive a future where the type
//! became reachable again under another name.
//!
//! # The property
//!
//! Three parts:
//!
//! 1. THE PAIR IS IN ONE PLACE. `type_text` is defined beside the `parse` it is the
//!    exact inverse of, in [`SPELLER_HOME`]. One contract, both directions, one file,
//!    so a change to either half meets the other.
//! 2. NO FORMER HOME KEPT A COPY. Two of them now: the engine file the renderer
//!    started in, and the neutral `snapshot.rs` it passed through. A leftover in
//!    either is a second physical home for MySQL type text, and the two will drift.
//! 3. THE ENGINE REACHES IT FROM NOWHERE. Nothing under `crates/zeroship-migrate-core/src`
//!    may spell a MySQL type. A vendor crate spelling for itself is fine and
//!    deliberately unconstrained — it is the only thing a vendor is for.
//!
//! # The floors, because a scan over a DISCOVERED set fails OPEN
//!
//! Narrow the walk and it iterates nothing and reports clean; break the needle and it
//! reads every file and matches nothing. A bare count floor catches neither reliably —
//! this tree has already had a narrowed walk pass a floor — so the walk carries a
//! SUBJECT ANCHOR as well, and the needle carries a POSITIVE CONTROL.
//!
//! The positive control had to move with the speller. It used to be "the one allowed
//! engine file makes at least two calls", which cannot survive an assertion that no
//! engine file makes any: a needle proven only by the thing it is asserting to be
//! absent proves nothing. It is now a count in [`SPELLER_HOME`], which is where the
//! matches are supposed to be.

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

/// The one physical home: the vendor crate that owns the type.
const SPELLER_HOME: &str = "crates/zeroship-migrate-mysql/src/physical_type.rs";

/// The two files the speller LEFT, in order. `drift.rs` is where the renderer was a
/// module-private `fn`; `snapshot.rs` is the neutral crate it lived in while the type
/// was still a field on `ColumnSnapshot`.
const FORMER_SPELLER_HOMES: [&str; 2] = [
    "crates/zeroship-migrate-core/src/apply/drift.rs",
    "crates/zeroship-migrate-backend/src/snapshot.rs",
];

/// The ident of the renderer that used to live in the former home. Its ABSENCE there
/// is half the anchor; a leftover copy would be a second physical home for type text
/// whose whole discipline is having exactly one.
const FORMER_SPELLER: &str = "format_mysql_physical_type";

/// The engine file this census is ABOUT, workspace-relative — the walk's subject
/// anchor, not an allowance.
///
/// `column_data_type_report` is here. It prints the two sides of a `data_type` drift
/// line, and it used to spell them: it held a `MysqlPhysicalType` and asked it for its
/// text. It now asks the carrier leg for a finished pair, so this file must contain
/// ZERO matches. Naming it is what proves the walk reached the one file whose zero
/// carries meaning — a walk above its floor can still miss its subject.
const ENGINE_SUBJECT_FILE: &str = "apply/drift.rs";

/// The walk's floor over `crates/zeroship-migrate-core/src` — the WEAKER of this file's two
/// anti-blindness checks, and the one to distrust first.
///
/// A floor ALONE is not enough and this one is not trusted alone: a narrowed walk in
/// this tree has already come in ABOVE its floor. [`ENGINE_SUBJECT_FILE`] is
/// asserted to be IN the walked set, by path, which is the check a count cannot make.
/// That anchor is what actually holds; this number only catches a walk that collapsed
/// toward nothing.
///
/// **The instruction this comment used to carry was backwards, and the sibling census
/// `core_names_no_vendor_crate.rs` already says why.** It read "raise it deliberately as
/// the engine grows" — but the engine is deliberately SHRINKING. Vendor code is being
/// moved out of it on purpose: `crates/zeroship-migrate-core/src` fell 79 -> 77 files in the two
/// commits that landed underneath this one, and `apply/backend/**` is ~30 more files
/// scheduled to leave. A count that falls is the project WORKING, so a floor written to
/// catch growth will fire on a correct tree, and "never lower it to get green" would
/// then forbid the only correct response.
///
/// The rule is narrower than it looks. LOWER it when an extraction legitimately moved
/// files OUT, and name that extraction in the commit. NEVER lower it to silence a walk
/// that broke - check [`ENGINE_SUBJECT_FILE`] first, because that is what tells
/// the two cases apart, and trust it over this number.
///
/// It fired exactly that way twice, on the extractions it was written in anticipation
/// of.
///
/// - `apply/backend/mysql/` — eight files, the whole MySQL execution half — became
///   `zeroship-migrate-mysql/src/backend/`, taking core from 77 `.rs` files to 69.
///   Lowered to 60.
/// - `apply/backend/sqlite/` — ELEVEN files, the whole SQLite execution half —
///   became `zeroship-migrate-sqlite/src/backend/`, taking core from 69 to 58.
///   Lowered to 50.
///
/// - `apply/backend/postgres/` — TEN files, the whole PostgreSQL execution half —
///   became `zeroship-migrate-postgres/src/backend/`, taking core from 58 to 48.
///   Lowered to 42.
///
/// All three times the anchor passed, which is what said the walk was intact and the
/// shrink was the move. 42 is still far from a walk that collapsed toward nothing.
/// There is no backend subtree left under `apply/backend/` to take it lower again.
const ENGINE_FILE_FLOOR: usize = 42;

/// The needle's positive control: lines naming the speller in [`SPELLER_HOME`].
///
/// The definition, its doc links and the two calls `type_drift_report` makes, one per
/// side. If this goes to zero the needle has stopped matching and the zeroes over the
/// engine mean nothing. A drop is a QUESTION — either the needle broke (fix the
/// needle) or the vendor stopped spelling its own contract (lower this deliberately,
/// in the commit that does it).
///
/// It must NOT be anchored on the engine any more. The old control counted calls in
/// the one engine file that was allowed to make them; this census now asserts that no
/// engine file makes any, and a needle whose only proof of life is the thing being
/// asserted absent proves nothing at all.
const SPELLER_HOME_LINE_FLOOR: usize = 4;

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
    let home = include_str!("../../../zeroship-migrate-mysql/src/physical_type.rs");
    let former_drift = include_str!("../../../zeroship-migrate-core/src/apply/drift.rs");
    let former_snapshot = include_str!("../../../zeroship-migrate-backend/src/snapshot.rs");

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

    // FLOOR ZERO, the NEEDLE, as a positive control where the matches belong.
    let home_lines = home
        .lines()
        .filter(|l| is_code(l))
        .filter(|l| l.contains(SPELLER))
        .count();
    assert!(
        home_lines >= SPELLER_HOME_LINE_FLOOR,
        "the positive control found {home_lines} `{SPELLER}` line(s) in \
         {SPELLER_HOME}, below the floor of {SPELLER_HOME_LINE_FLOOR}. Either the \
         needle stopped matching - in which case every zero below is meaningless - or \
         the vendor stopped spelling its own contract, which is a deliberate change \
         this floor should be lowered for in the same commit."
    );

    // ANCHOR, side two: neither former home kept a copy.
    for (former, text) in FORMER_SPELLER_HOMES
        .iter()
        .zip([former_drift, former_snapshot])
    {
        assert!(
            !text.contains(FORMER_SPELLER),
            "{former} still names `{FORMER_SPELLER}`. That renderer is \
             `MysqlPhysicalType::{SPELLER}` in {SPELLER_HOME}; a copy left behind is a \
             second physical home for MySQL type text, and the two will drift."
        );
        assert!(
            !text.contains(SPELLER),
            "{former} still names `{SPELLER}`. The engine and the neutral backend \
             vocabulary both ASK a vendor leg for a finished pair of type spellings \
             now; naming the speller from either is the coupling this census exists to \
             report."
        );
    }

    let engine_src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("this crate lives at <workspace>/crates/<name>")
        .join("zeroship-migrate-core")
        .join("src");
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
        walked.iter().any(|p| p == ENGINE_SUBJECT_FILE),
        "the walk did not reach {ENGINE_SUBJECT_FILE}, the one file this census is \
         actually about. A walk above its floor can still miss its subject - that has \
         happened in this tree - so the subject is named rather than counted."
    );

    // THE PROPERTY: a flat zero, with no allowance.
    let strays: Vec<_> = callsites(&files, &engine_src)
        .iter()
        .map(|(f, n)| format!("  {f}: {n} line(s)"))
        .collect();
    assert!(
        strays.is_empty(),
        "the engine spells a MySQL type:\n{}\n\n\
         `format_mysql_physical_type` was module-private to `apply::drift` and the \
         compiler enforced this; across a crate boundary `MysqlPhysicalType::{SPELLER}` \
         has to be `pub`, and this census is what stands in for the modifier. The \
         engine no longer even COMPARES a physical contract - it asks the carrier leg, \
         and the vendor both judges and spells. See `core_does_not_spell_a_vendors_\
         bytes.rs` for the rule and `zeroship_migrate::render::backends`'s header for why.",
        strays.join("\n")
    );
}
