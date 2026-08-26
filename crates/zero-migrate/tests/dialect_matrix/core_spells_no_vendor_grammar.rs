//! Core names no vendor — and now core does not SPELL one either.
//!
//! # Why a second census
//!
//! `core_names_no_vendor_at_all` bounds how much vendor NAMING escapes into the engine:
//! the strings `postgres`, `mysql`, `sqlite`, `pg_`. Its allowance list is empty and it
//! is a real guard. But three independent architecture reads converged on the same gap
//! in it, and the tree had already admitted the gap in one comment before they did:
//! a census over product names says nothing about vendor GRAMMAR.
//!
//! A neutral-looking function that writes `EXCLUDE USING gist (...)` names no vendor and
//! passes that census cleanly. It is still PostgreSQL semantics living in the engine,
//! and it is the shape the neutrality rule exists to prevent — vendor knowledge moving
//! out of the names and into the strings.
//!
//! # What counts as a needle here
//!
//! Only SQL that ONE shipping vendor can execute. `DEFERRABLE` is deliberately NOT in
//! the list: SQLite accepts it on a foreign key, so its presence in core is not by
//! itself evidence of a leak, and a needle that fires on portable grammar trains people
//! to add allowances. The exclusion-constraint body that spells `DEFERRABLE` is caught
//! by `EXCLUDE USING` instead, which no other vendor has.
//!
//! Two matchers, because these split on case in opposite directions. SQL keywords are
//! written upper-case in this tree and are matched that way; catalogue and access-method
//! identifiers (`pg_catalog`, `spgist`, `sqlite_master`) are written lower-case and are
//! matched case-folded. Running only one of them would miss half the surface, so
//! [`GRAMMAR_MATCH_FLOOR`] is checked against their COMBINED answer and each is
//! separately exercised by the control below.
//!
//! # The floors, because a scan over a DISCOVERED set fails OPEN
//!
//! Narrow the walk and it iterates nothing; break the needles and it reads every file
//! and matches nothing. Both blindnesses look identical to a pass, so:
//!
//! - [`SRC_FILE_FLOOR`] and [`WALK_ANCHORS`] defend the walk — how many files, and
//!   WHICH files, since a count alone stays true at any crate size.
//! - [`GRAMMAR_MATCH_FLOOR`] defends the needles, by running the IDENTICAL matcher over
//!   a crate where the grammar is SUPPOSED to live and requiring it to find plenty.
//!
//! # The allowance may only shrink
//!
//! [`ALLOWED`] carries one entry, and it is debt rather than an exemption. Do not add to
//! it to make a change pass; move the spelling to the backend that owns it, the way the
//! ALTER COLUMN family and the SQLite trigger family already were.

use std::collections::BTreeSet;
use std::path::Path;

// The walk, the prose stripper and the `#[cfg(test)]` splitter are the sibling census's,
// reused rather than rewritten. That splitter is not a brace matcher: its own doc records
// that a brace matcher mis-split 78 lines here, because `render/lower.rs` carries a
// one-line `#[cfg(test)] fn` two thousand lines above its real test module. A second
// hand-rolled copy would have re-earned that bug.
use crate::core_names_no_vendor_at_all::{
    cfg_test_module_files, code_of, crate_src, rust_files, test_line_span,
};

/// The engine crate. Cargo already forbids it from DEPENDING on a vendor; this file is
/// about what it writes, which Cargo cannot see.
const ENGINE_CRATE: &str = "zero-migrate-core";

/// The crate the needles are proved live against: grammar SHOULD be dense here.
const NEEDLE_CONTROL_CRATE: &str = "zero-migrate-postgres";

/// Upper-case SQL keywords only one shipping vendor can execute.
const KEYWORD_NEEDLES: &[&str] = &[
    "EXCLUDE USING",
    "AUTO_INCREMENT",
    "AUTOINCREMENT",
    "WITHOUT ROWID",
    "SHOW CREATE",
    "BIGSERIAL",
    "TSVECTOR",
    "ILIKE",
    "PRAGMA",
    "CONCURRENTLY",
];

/// Lower-case vendor identifiers: catalogues and index access methods.
const IDENT_NEEDLES: &[&str] = &["pg_catalog", "sqlite_master", "spgist"];

/// Recorded debt. `(file, count, why)`. MAY ONLY SHRINK.
///
/// `plan/author.rs` spells `CONCURRENTLY` twice. That file was measured separately and
/// found to be PostgreSQL-shaped by construction rather than PostgreSQL-shaped by
/// accident — it is a known, logged carve-out, not a leak this census discovered. It is
/// listed rather than excluded from the walk so that the debt stays visible and so that
/// a THIRD `CONCURRENTLY` appearing in it still fails here.
const ALLOWED: &[(&str, usize, &str)] = &[(
    "plan/author.rs",
    2,
    "PostgreSQL-shaped by construction; carve-out tracked separately, not a new leak",
)];

/// The walk's weaker anti-blindness check. Core held 47 `.rs` files when this was
/// written, after the vendor extractions. Never lower this to silence a walk that broke.
const SRC_FILE_FLOOR: usize = 30;

/// Files the walk MUST reach. A count bounds HOW MANY were found; these bound WHICH,
/// and stay true at any crate size.
const WALK_ANCHORS: &[&str] = &["lib.rs", "render/lower.rs"];

/// Needle liveness: the identical matcher, run over the PostgreSQL crate, must find at
/// least this many. Break a needle and this floor fails before a false green can.
const GRAMMAR_MATCH_FLOOR: usize = 25;

/// Does this fragment spell an upper-case vendor-only keyword?
fn spells_keyword(code: &str) -> bool {
    KEYWORD_NEEDLES.iter().any(|n| code.contains(n))
}

/// Does this fragment spell a lower-case vendor identifier? Case-folded, because these
/// appear as `pg_catalog` in SQL and `PG_CATALOG` in nothing.
fn spells_ident(code: &str) -> bool {
    let lowered = code.to_ascii_lowercase();
    IDENT_NEEDLES.iter().any(|n| lowered.contains(n))
}

/// The combined matcher. The floor is checked against THIS, never against one half, so
/// a corrupted keyword list cannot be masked by a healthy identifier list.
fn spells_vendor_grammar(code: &str) -> bool {
    spells_keyword(code) || spells_ident(code)
}

/// Walk `src`, returning `(files_seen, relative_paths_seen, hits)` where a hit is
/// `(relative path, line number, the offending code)`.
///
/// Test code is EXCLUDED, both whole `#[cfg(test)] mod x;` files and in-file
/// `#[cfg(test)]` spans. That exclusion is the point rather than a convenience: a test
/// asserting what PostgreSQL renders MUST spell PostgreSQL, so counting it would make
/// the census unpassable and the only way out would be allowances. What is being bounded
/// is the SQL the engine SHIPS.
fn census(src: &Path) -> (usize, BTreeSet<String>, Vec<(String, usize, String)>) {
    let files = rust_files(src);
    let test_only = cfg_test_module_files(src);
    let mut seen = BTreeSet::new();
    let mut hits = Vec::new();
    for file in &files {
        let rel = file
            .strip_prefix(src)
            .expect("walked under src")
            .to_string_lossy()
            .replace('\\', "/");
        seen.insert(rel.clone());
        if test_only.contains(file) {
            continue;
        }
        let text = std::fs::read_to_string(file)
            .unwrap_or_else(|error| panic!("read {}: {error}", file.display()));
        let in_test = test_line_span(&text);
        for (n, line) in text.lines().enumerate() {
            if in_test.contains(&n) {
                continue;
            }
            let Some(code) = code_of(line) else { continue };
            if spells_vendor_grammar(code) {
                hits.push((rel.clone(), n + 1, code.trim().to_string()));
            }
        }
    }
    (files.len(), seen, hits)
}

/// The engine spells no vendor-only SQL beyond its one recorded carve-out.
#[test]
fn core_spells_no_vendor_grammar_outside_its_recorded_debt() {
    let src = crate_src(ENGINE_CRATE);
    let (file_count, seen, hits) = census(&src);

    assert!(
        file_count >= SRC_FILE_FLOOR,
        "the walk found only {file_count} files under {}, below the floor of \
         {SRC_FILE_FLOOR}. A census over a DISCOVERED set fails OPEN, so too few files \
         is a broken instrument reporting a clean tree, not a clean tree.",
        src.display()
    );
    for anchor in WALK_ANCHORS {
        assert!(
            seen.contains(*anchor),
            "the walk never reached `{anchor}`, so whatever it did reach is not the \
             engine. Found {file_count} files. A file count alone cannot catch this."
        );
    }

    let mut unexplained: Vec<&(String, usize, String)> = Vec::new();
    for hit in &hits {
        let budget = ALLOWED
            .iter()
            .find(|(file, _, _)| *file == hit.0)
            .map_or(0, |(_, count, _)| *count);
        let used = hits.iter().filter(|h| h.0 == hit.0).count();
        if used > budget {
            unexplained.push(hit);
        }
    }

    assert!(
        unexplained.is_empty(),
        "the engine SPELLS vendor-only SQL. Naming no vendor is not enough — this is \
         the grammar the neutrality rule exists to keep out, and it passes the \
         product-name census untouched.\n\n{}\n\nMove the spelling to the backend that \
         owns it, the way the ALTER COLUMN family and the SQLite trigger family already \
         were. Do NOT widen `ALLOWED`; it may only shrink.",
        unexplained
            .iter()
            .map(|(f, n, code)| format!("  {f}:{n}: {code}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The needles find grammar where grammar is supposed to be.
///
/// Without this the census could pass by matching nothing at all: a typo in one needle,
/// a dropped space in `EXCLUDE USING`, a case slip. Running the IDENTICAL matcher over
/// the PostgreSQL crate turns that failure into a red.
#[test]
fn the_grammar_needles_are_live() {
    let src = crate_src(NEEDLE_CONTROL_CRATE);
    let (_, _, hits) = census(&src);

    assert!(
        hits.len() >= GRAMMAR_MATCH_FLOOR,
        "the identical matcher found only {} vendor-grammar lines in \
         `{NEEDLE_CONTROL_CRATE}`, below the liveness floor of {GRAMMAR_MATCH_FLOOR}. \
         The needles are broken, so the engine census above proves nothing.",
        hits.len()
    );
}

/// Each half of the matcher can find something on its own.
///
/// The combined matcher hides a corrupted half behind a healthy one — that is exactly
/// how a census goes quietly toothless. This pins that both halves still bite.
#[test]
fn both_halves_of_the_matcher_bite_separately() {
    let src = crate_src(NEEDLE_CONTROL_CRATE);
    let files = rust_files(&src);
    let test_only = cfg_test_module_files(&src);
    let (mut keyword, mut ident) = (0usize, 0usize);
    for file in &files {
        if test_only.contains(file) {
            continue;
        }
        let text = std::fs::read_to_string(file).expect("read control file");
        let in_test = test_line_span(&text);
        for (n, line) in text.lines().enumerate() {
            if in_test.contains(&n) {
                continue;
            }
            let Some(code) = code_of(line) else { continue };
            if spells_keyword(code) {
                keyword += 1;
            }
            if spells_ident(code) {
                ident += 1;
            }
        }
    }

    assert!(
        keyword > 0,
        "the UPPER-CASE keyword half matched nothing in `{NEEDLE_CONTROL_CRATE}`, so a \
         keyword leak in the engine could not be seen even while the census passed"
    );
    assert!(
        ident > 0,
        "the lower-case identifier half matched nothing in `{NEEDLE_CONTROL_CRATE}`, so \
         a `pg_catalog`/`spgist` leak in the engine could not be seen"
    );
}

/// The recorded debt names files that exist.
///
/// An allowance keyed on a path that no longer exists is a silent widening: it exempts
/// nothing, so it looks harmless, and it hides that the debt was already paid.
#[test]
fn the_allowance_names_only_files_that_exist() {
    let src = crate_src(ENGINE_CRATE);
    let (_, seen, _) = census(&src);
    for (file, _, why) in ALLOWED {
        assert!(
            seen.contains(*file),
            "`ALLOWED` exempts `{file}` ({why}), but the walk never saw that file. \
             Either the path is stale, in which case DELETE the entry rather than \
             leaving it, or the walk stopped reaching it."
        );
    }
}
