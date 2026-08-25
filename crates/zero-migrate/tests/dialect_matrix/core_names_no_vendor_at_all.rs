//! The RATCHET on the owner's governing rule — *the core should be neutral, this is
//! the hard limit* — applied to the ENGINE, over vendor PRODUCT NAMES rather than
//! vendor crate idents.
//!
//! # Why this file exists beside three siblings that already guard core
//!
//! `core_names_no_vendor_crate.rs` measures whether core writes
//! `zero_migrate_postgres` / `_sqlite` / `_mysql` — whether core RESOLVES a vendor
//! outside the registry. `core_does_not_spell_a_vendors_bytes.rs` measures whether it
//! calls a raw spelling primitive. `core_names_no_vendor_backend_module.rs` measures
//! the module paths. All three were green while the engine still said "PostgreSQL" in
//! code 154 times, because none of them looks for the product name.
//!
//! That gap was not theoretical and it was not cosmetic. What the sweep behind this
//! file found, and the commits it lands with fixed:
//!
//! * `GENERATED_IDENT_MAX_BYTES` — the one budget every generated constraint, index
//!   and trigger name is minted against — read `POSTGRES_VENDOR`'s declared cap while
//!   the doc directly above it said it read "the registered backend that imposes the
//!   limiting byte-counted cap". A backend declaring a tighter cap was read past.
//! * `authored_name_within_bound` told EVERY target "PostgreSQL truncates identifiers
//!   to 63 bytes", on a check with no dialect gate. False on MySQL, and false on
//!   SQLite, which declares `IdentifierLimit::Unbounded` and truncates nothing.
//! * `schema::query`'s "platform-reserved" name table held one BACKEND's catalog
//!   namespace (`sqlite_`), and `validate_collection` held another's (`pg_`) as a
//!   hard-coded byte comparison.
//! * `db_url::is_sqlite_url` decided which backend a URL selects by string-matching
//!   schemes, without the registry. It had zero callers anywhere in the workspace.
//! * The managed dual-write trigger was spelled in the engine —
//!   `CREATE OR REPLACE FUNCTION … LANGUAGE plpgsql`, `CREATE TRIGGER …`, and twice
//!   `DROP TRIGGER <t> ON <table>` — with its PL/pgSQL body one crate lower still, in
//!   the neutral CONTRACT.
//!
//! # What this census can and cannot see, stated because it MATTERS here
//!
//! It matches vendor product NAMES. The dual-write body above is the standing
//! counter-example: twenty lines of `TG_OP` / `NEW` / `OLD` / `RETURN NEW` sat in
//! `zero-migrate-backend` and passed that crate's identical census for its whole life
//! there, because PL/pgSQL contains no product name. A census over names bounds how
//! much vendor NAMING escapes, not how much vendor GRAMMAR does. Do not read a green
//! here as "core holds no vendor"; read it as "core writes no vendor's name".
//!
//! # Prose is not a name
//!
//! [`code_of`] strips comment lines and the trailing half of a line comment first, for
//! the reason every sibling states: this tree's prose is dense with vendor names ON
//! PURPOSE, and a doc saying WHICH server was measured, or which backend populates a
//! fact, is the valuable part. A `#[error]` string or a `format!` is NOT prose — it is
//! a value core emits at whatever target reached the arm, and that is where the two
//! actively-false messages above lived.
//!
//! # The TEST HALF is excluded, and getting that wrong cost a 3.5x miscount
//!
//! A test that targets PostgreSQL names PostgreSQL, and must. So this census counts
//! PRODUCTION lines only — and "production" is a harder question than it looks. The
//! brief that commissioned this pass measured **545** production hits. The real number
//! was **154**, and the entire 391-line difference was two classification bugs, not a
//! difference of needle:
//!
//! * **313** lines were in files that are ENTIRELY test code because their `mod`
//!   declaration in the PARENT file carries `#[cfg(test)]` —
//!   `render/gen_types/differential_corpus.rs` (296 on its own),
//!   `render/gen_types/fold_projection_equality.rs`, `model/support_matrix.rs`. Read
//!   one of those files alone and nothing in it says it is a test. The brief's biggest
//!   single "violation", and the one it asked to have redesigned around the registry,
//!   was a `#[cfg(test)] mod` that ships in nothing.
//! * **78** lines were in-file `#[cfg(test)]` spans that a brace matcher mis-split,
//!   almost all of them in `render/lower.rs`, whose test module is 5,600 lines long
//!   and is NOT the file's first `#[cfg(test)]` — a one-line `#[cfg(test)] fn`
//!   helper sits 2,100 lines above it.
//!
//! Two functions handle those two shapes: [`cfg_test_module_files`] resolves the parent's
//! `#[cfg(test)] mod x;` to a file and closes over it transitively, and
//! [`test_line_span`] brace-matches EVERY `#[cfg(test)]` in a file with a lexer that
//! understands raw strings, char literals versus lifetimes, and block comments. A
//! naive matcher gets all three wrong, and `&'a str` alone is enough to make one
//! swallow the rest of a file.
//!
//! # The floors, because a scan over a DISCOVERED set fails OPEN
//!
//! Narrow the walk and it iterates nothing. Break the needle and it reads every file
//! and matches nothing. Two blindnesses, one green, so two floors:
//! [`WALK_ANCHORS`] plus [`SRC_FILE_FLOOR`] defend the walk, and
//! [`VENDOR_CRATE_MATCH_FLOOR`] defends the needle by running the IDENTICAL matcher
//! over a vendor crate, where naming the vendor is the entire point and a zero could
//! only mean the needle died.
//!
//! The needle control is deliberately NOT the [`ALLOWED`] entries. Those are expected
//! to reach zero, and a census whose positive control is its own remaining violations
//! goes blind at exactly the moment it succeeds.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The vendor product names, lowercased. A line is matched against its own lowercased
/// code, so `MySql`, `MYSQL` and `mysql` are one needle.
///
/// `pg_` carries the underscore because a bare `pg` matches inside ordinary words.
/// `postgre` is not listed separately: it is a prefix of `postgres`, and a line is
/// counted once however many needles hit it.
const VENDOR_NEEDLES: &[&str] = &["mysql", "sqlite", "postgres", "pgsql", "pg_", "mariadb"];

/// The ratchet: which core files may name a vendor in PRODUCTION code, and exactly how
/// many times.
///
/// One more is a red, and one FEWER is also a red — a count that silently drifts down
/// is a count nobody is maintaining. A file not listed here may not name a vendor at
/// all. Lowering an entry is the point of this file; RAISING one, or adding a file, is
/// what it exists to make loud.
///
/// Every entry carries its ARGUMENT, not just a number, because a bare count tells the
/// next reader nothing about whether it may come off.
const ALLOWED: &[(&str, usize, &str)] = &[];

// `render/backends/mod.rs` WAS THE LAST ENTRY, at four, and it is GONE rather than
// lowered. The ratchet is EMPTY: no file in the engine's production source names a
// vendor at all.
//
// Its four were the `POSTGRES_VENDOR` / `SQLITE_VENDOR` / `MYSQL_VENDOR` consts and
// the `SHIPPING` array that listed them — the registry composition, described here as
// "PERMANENT, and the one place designed to hold this". It was permanent in the crate
// it was in. The crate split moved it to `crates/zero-migrate/src/lib.rs`, a crate
// whose entire job is to hold that knowledge, and `zero-migrate-core` stopped
// declaring a vendor dependency at all.
//
// So this entry did not close the way the three above it did, by finding a neutral
// formulation for a coupling. It closed by MOVING the coupling to a crate where it is
// not a coupling, and the engine now cannot name a vendor even if someone tries: the
// idents do not resolve. A fourth backend is a `[dependencies]` line in that manifest
// plus an entry in that array, with no edit to the contract crate, the engine, or any
// other vendor.
//
// AN EMPTY RATCHET IS NOT A RETIRED FILE. This census matches vendor PRODUCT NAMES —
// "PostgreSQL", "SQLite", "MySQL" and their spellings — not crate idents, and Cargo
// cannot see a product name in a diagnostic string, a `format!`, or a hard-coded
// catalog prefix. The four things the sweep behind this file found were all of that
// shape, and every one of them would still compile today.

// `lib.rs` USED TO BE THE SECOND ENTRY, at one, and it is GONE rather than lowered.
//
// The line was `pub use zero_migrate_ir::dialect::{DialectId, DialectSet, MYSQL,
// POSTGRES, SQLITE}` — core re-exporting three id constants it does not define. The
// violation was UPSTREAM of core and that line was its symptom: the constants lived
// in `zero-migrate-ir/src/dialect.rs`, a crate whose own doc says a backend "declares
// its own — `DialectId::new(\"duckdb\")` — without editing this crate", while itself
// declaring three.
//
// The end state that entry named is the one that landed: each VENDOR crate exports
// its own id and the IR crate exports none. `zero_migrate_postgres::DIALECT`,
// `zero_migrate_sqlite::DIALECT` and `zero_migrate_mysql::DIALECT` are the
// declarations; core re-exports `DialectId` and `DialectSet` from that module and
// nothing else.
//
// TWO CLAIMS IN THAT ARGUMENT WERE WRONG, and both were arguments for deferring:
//
// * "add a `zero-migrate-ir` dependency to the Node addon, which has none". The
//   addon already carries all three VENDOR crates in `[dependencies]` and already
//   names two of their backend types in `bridge.rs`. It gained no dependency and
//   needed no `zero-migrate-ir`.
// * "relocate the same three names from one neutral crate to another". They did not
//   go to another neutral crate. They went to the three crates that ARE those
//   vendors, which is the only move that makes the bottom of the stack neutral.
//
// The size was right in shape and low in magnitude: 2,583 references across 287
// files, not 2,356 across 286 — the smaller number counted matching LINES, and 224
// lines name more than one id.
//
// WHAT CORE'S OWN `src` DOES INSTEAD is the part worth knowing before anyone
// "simplifies" it. Twenty-two `#[cfg(test)]` modules under `src/` name a dialect and
// they do NOT read the vendor crates: `core_names_no_vendor_crate.rs` counts every
// non-comment line under `src/`, test lines included, and
// `core_names_no_vendor_backend_module.rs` allows `postgres::` in path position
// nowhere. They read three `DialectId::new` consts in `crate::test_fixtures`, which
// this census already excludes as a `#[cfg(test)] mod` file. Repointing them at the
// vendors would trade this entry for forty-eight in the two sibling censuses.

/// The walk's floor — the WEAKER of the two anti-blindness checks. See [`WALK_ANCHORS`]
/// for the one that actually holds.
///
/// Core is deliberately SHRINKING: vendor code is moved out of it on purpose, and this
/// file's sibling `core_names_no_vendor_crate.rs` has already lowered its own floor
/// three times for three extractions. So a count that falls is often the project
/// working, and the rule is narrower than "never lower it": lower it when an
/// extraction legitimately moved files OUT, and name the extraction in the commit.
/// Never lower it to silence a walk that broke — [`WALK_ANCHORS`] is what tells those
/// two apart, so check it first and trust it over this number.
///
/// Measured at 47 `.rs` files under `src` when this landed — `db_url.rs` left in the
/// same pass, having had zero callers, taking the sibling census's own recorded 48 down
/// by one. 36 sits under that with room for churn and nowhere near a walk that found
/// nothing.
const SRC_FILE_FLOOR: usize = 36;

/// Files the walk MUST reach, which is the real defence against a census that fails
/// open.
///
/// A floor over a discovered set bounds only HOW MANY things were found; these bound
/// WHICH, and stay true at any crate size. `lib.rs` sits at the walk's root, so losing
/// it means the root was wrong. `render/backends/mod.rs` is three levels down and is
/// the registry, so losing it means recursion stopped descending.
///
/// Pick replacements only from files that cannot move.
const WALK_ANCHORS: &[&str] = &["lib.rs", "render/backends/mod.rs"];

/// The NEEDLE-LIVENESS floor: matches the identical matcher must find in a crate where
/// naming the vendor is the whole point.
///
/// Break `VENDOR_NEEDLES` — a typo, a dropped underscore, a case slip — and the walk
/// above still visits every file, still reads every line, and still reports zero
/// violations, because it finds zero of anything. Set low and blunt on purpose: the
/// number only has to prove the matcher is alive.
const VENDOR_CRATE_MATCH_FLOOR: usize = 40;

/// The liveness floor for the SECOND matcher, `Pg` followed by an uppercase letter.
///
/// Separate from [`VENDOR_CRATE_MATCH_FLOOR`] because the two matchers must be
/// controlled independently — see [`names_a_vendor`]. Measured well above this in
/// `zero-migrate-postgres`, which names types `PgRaw`, `PgDml` and so on throughout.
const PG_CAMEL_MATCH_FLOOR: usize = 20;

/// The vendor crate the needle control runs over.
const NEEDLE_CONTROL_CRATE: &str = "zero-migrate-postgres";

/// Whether a source line is CODE rather than a comment, and the code half of it.
///
/// Line-oriented on purpose, with the same stated limit as its siblings: it cannot see
/// inside a block comment that starts mid-line and does not try. It over-counts prose
/// into code, never the reverse, which is the safe direction for a census asserting a
/// bound.
pub(crate) fn code_of(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
        return None;
    }
    Some(match line.find("//") {
        Some(at) => &line[..at],
        None => line,
    })
}

/// Does this code fragment contain one of [`VENDOR_NEEDLES`], case-folded?
fn names_a_vendor_by_needle(code: &str) -> bool {
    let lowered = code.to_ascii_lowercase();
    VENDOR_NEEDLES.iter().any(|n| lowered.contains(n))
}

/// Does this code fragment contain `Pg` followed by an uppercase letter?
///
/// Checked on the RAW fragment because this spelling does not survive lowercasing:
/// `PgOnly` lowercases to `pgonly`, which contains no needle, and that exact blind
/// spot is how four references to a DELETED `DialectScope::PgOnly` variant survived a
/// sweep of the contract crate that reported clean.
fn names_a_vendor_by_pg_camel(code: &str) -> bool {
    code.as_bytes()
        .windows(3)
        .any(|w| w[0] == b'P' && w[1] == b'g' && w[2].is_ascii_uppercase())
}

/// Does this code fragment contain `PG` as a WORD — the all-caps spelling?
///
/// The third matcher, and it exists because the first two left a gap between them that
/// a live violation sat in. `crate::model::validate`'s vendor-op refusal read
/// **"vendor PG primitive (op capability …)"** — a product name in an operator-facing
/// string, in the crate that must not name one — and every census reported clean:
///
/// * [`names_a_vendor_by_needle`] case-folds and looks for `pg_`, WITH the underscore.
///   `PG ` lowercases to `pg `, a space, so no needle matched.
/// * [`names_a_vendor_by_pg_camel`] requires a lowercase `g` (`PgOnly`, `PgRaw`).
///   `PG` has an uppercase one, so it did not match either.
///
/// Two matchers tuned to two real spellings, and the third spelling went straight
/// between them. Word-bounded on both sides so `PGN`, `SPG` and a base64 blob that
/// happens to contain the pair do not fire.
fn names_a_vendor_by_pg_word(code: &str) -> bool {
    let bytes = code.as_bytes();
    bytes.windows(2).enumerate().any(|(i, w)| {
        if w[0] != b'P' || w[1] != b'G' {
            return false;
        }
        let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_';
        let after = bytes.get(i + 2);
        let after_ok = after.is_none_or(|c| !c.is_ascii_alphanumeric() && *c != b'_');
        before_ok && after_ok
    })
}

/// Does this code fragment name a vendor, by either spelling?
///
/// The two are kept as SEPARATE functions rather than one `||` because they have to be
/// controlled separately, and finding that out was itself a measurement. The first
/// version of [`the_needle_still_matches_where_a_vendor_name_is_the_point`] asserted
/// one floor over this combined answer, and when the needle list was corrupted to
/// prove the control fires, IT PASSED: `zero-migrate-postgres` is full of `PgRaw`,
/// `PgDml` and friends, so the camel matcher alone cleared the floor while every
/// product-name needle was dead. A positive control that two matchers can satisfy for
/// each other is not a control over either.
fn names_a_vendor(code: &str) -> bool {
    names_a_vendor_by_needle(code)
        || names_a_vendor_by_pg_camel(code)
        || names_a_vendor_by_pg_word(code)
}

/// One structural token: a brace outside any string, char or comment, or the start of a
/// `#[cfg(test)]` attribute.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Token {
    Open,
    Close,
    CfgTest,
}

/// Lex `text` for [`Token`]s, skipping strings, raw strings, char literals and
/// comments.
///
/// Hand-written rather than regex because all three of the things it must skip are
/// things a naive scan gets wrong, and each one has a concrete failure here:
///
/// * a raw string `r#"…{…"#` holds unbalanced braces in ordinary SQL fixtures;
/// * `'` is a lifetime far more often than a char literal in this tree, and treating
///   `&'a str` as opening a literal swallows every brace to the next apostrophe;
/// * a block comment can hold anything at all.
fn lex(text: &str) -> Vec<(usize, Token)> {
    let bytes = text.as_bytes();
    let end = bytes.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < end {
        let byte = bytes[i];
        // line comment
        if byte == b'/' && i + 1 < end && bytes[i + 1] == b'/' {
            i = text[i..].find('\n').map_or(end, |at| i + at);
            continue;
        }
        // block comment, nesting (Rust's are nestable)
        if byte == b'/' && i + 1 < end && bytes[i + 1] == b'*' {
            let mut depth = 1usize;
            i += 2;
            while i < end && depth > 0 {
                if bytes[i] == b'/' && i + 1 < end && bytes[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && i + 1 < end && bytes[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        // raw / byte-raw string: r"…", r#"…"#, br##"…"##
        if byte == b'r' || byte == b'b' {
            let mut scan = i;
            if bytes[scan] == b'b' && scan + 1 < end && bytes[scan + 1] == b'r' {
                scan += 1;
            }
            if bytes[scan] == b'r' {
                let mut probe = scan + 1;
                let mut hashes = 0usize;
                while probe < end && bytes[probe] == b'#' {
                    hashes += 1;
                    probe += 1;
                }
                if probe < end && bytes[probe] == b'"' {
                    let mut close = String::with_capacity(hashes + 1);
                    close.push('"');
                    for _ in 0..hashes {
                        close.push('#');
                    }
                    i = text[probe + 1..]
                        .find(&close)
                        .map_or(end, |at| probe + 1 + at + close.len());
                    continue;
                }
            }
        }
        // ordinary string
        if byte == b'"' {
            i += 1;
            while i < end {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        // char literal vs lifetime
        if byte == b'\'' {
            if i + 1 < end && bytes[i + 1] == b'\\' {
                // escaped char: run to the closing quote
                let mut scan = i + 2;
                while scan < end && bytes[scan] != b'\'' {
                    scan += 1;
                }
                i = scan + 1;
                continue;
            }
            if i + 2 < end && bytes[i + 2] == b'\'' {
                i += 3; // simple char literal
                continue;
            }
            i += 1; // lifetime
            continue;
        }
        match byte {
            b'{' => out.push((i, Token::Open)),
            b'}' => out.push((i, Token::Close)),
            b'#' if text[i..].starts_with("#[cfg(test)]") => out.push((i, Token::CfgTest)),
            _ => {}
        }
        i += 1;
    }
    out
}

/// Byte offset -> 0-based line index.
fn line_index(text: &str) -> impl Fn(usize) -> usize + '_ {
    let mut starts = vec![0usize];
    for (at, ch) in text.char_indices() {
        if ch == '\n' {
            starts.push(at + 1);
        }
    }
    move |pos| starts.partition_point(|&s| s <= pos).saturating_sub(1)
}

/// Every 0-based line index inside a `#[cfg(test)]` item in `text`.
///
/// Brace-matched from EVERY `#[cfg(test)]`, not the first: `render/lower.rs` has a
/// one-line `#[cfg(test)] fn` helper 2,100 lines above its 5,600-line test module, and
/// treating the first attribute as the boundary put 68 test lines in the production
/// column.
pub(crate) fn test_line_span(text: &str) -> BTreeSet<usize> {
    let tokens = lex(text);
    let line_of = line_index(text);
    let mut inside = BTreeSet::new();
    let mut at = 0;
    while at < tokens.len() {
        if tokens[at].1 != Token::CfgTest {
            at += 1;
            continue;
        }
        let start = tokens[at].0;
        // The item's first structural brace. A `#[cfg(test)] use a::{b, c};` has one
        // too, and enclosing exactly that group is the correct answer for it.
        let Some(open) = (at + 1..tokens.len()).find(|&k| tokens[k].1 == Token::Open) else {
            inside.insert(line_of(start));
            at += 1;
            continue;
        };
        let mut depth = 0usize;
        let mut close = tokens.len() - 1;
        for (offset, (_, token)) in tokens[open..].iter().enumerate() {
            match token {
                Token::Open => depth += 1,
                Token::Close => {
                    depth -= 1;
                    if depth == 0 {
                        close = open + offset;
                        break;
                    }
                }
                Token::CfgTest => {}
            }
        }
        for line in line_of(start)..=line_of(tokens[close].0) {
            inside.insert(line);
        }
        at = close + 1;
    }
    inside
}

/// Every `.rs` file under `dir`, recursively, in stable order.
pub(crate) fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let entries = std::fs::read_dir(&next)
            .unwrap_or_else(|error| panic!("read {}: {error}", next.display()));
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Resolve `mod name;` declared in `parent` to the file it names.
fn module_file(parent: &Path, name: &str) -> Option<PathBuf> {
    let dir = parent.parent()?;
    let stem = parent.file_stem()?.to_str()?;
    let roots: Vec<PathBuf> = if matches!(stem, "mod" | "lib" | "main") {
        vec![dir.to_path_buf()]
    } else {
        vec![dir.to_path_buf(), dir.join(stem)]
    };
    roots
        .into_iter()
        .flat_map(|root| {
            [
                root.join(format!("{name}.rs")),
                root.join(name).join("mod.rs"),
            ]
        })
        .find(|candidate| candidate.is_file())
}

/// `mod` names declared in `text`, paired with whether the declaration is
/// `#[cfg(test)]`.
///
/// Deliberately tolerant about what sits between the attribute and the `mod`: a
/// visibility, another attribute, a doc line. It reads the two preceding non-blank
/// lines, which covers every shape in this tree.
fn declared_mods(text: &str) -> Vec<(String, bool)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let Some(rest) = trimmed
            .strip_prefix("mod ")
            .or_else(|| trimmed.strip_prefix("pub mod "))
            .or_else(|| {
                trimmed
                    .strip_prefix("pub(crate) mod ")
                    .or_else(|| trimmed.strip_prefix("pub(super) mod "))
            })
        else {
            continue;
        };
        let Some(name) = rest.strip_suffix(';') else {
            continue; // `mod x { … }`, an inline module, not a file
        };
        let name = name.trim();
        if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let gated = lines[i.saturating_sub(2)..i]
            .iter()
            .any(|prior| prior.trim() == "#[cfg(test)]");
        out.push((name.to_string(), gated));
    }
    out
}

/// Files that are ENTIRELY test code because the `mod` declaration that brings them in
/// is `#[cfg(test)]`, transitively.
///
/// This is the check whose absence turned a 154-line census into a 545-line one. There
/// is nothing inside `render/gen_types/differential_corpus.rs` — 296 vendor-naming
/// lines, the single biggest concentration in the crate — that says it is a test. The
/// fact lives one file away, in `render/gen_types.rs`, as `#[cfg(test)] mod
/// differential_corpus;`.
pub(crate) fn cfg_test_module_files(root: &Path) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    let mut frontier: Vec<PathBuf> = Vec::new();
    for path in rust_files(root) {
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        for (name, gated) in declared_mods(&text) {
            if gated {
                if let Some(file) = module_file(&path, &name) {
                    if out.insert(file.clone()) {
                        frontier.push(file);
                    }
                }
            }
        }
    }
    // A cfg(test) module's own children are test code too.
    while let Some(current) = frontier.pop() {
        let text = std::fs::read_to_string(&current)
            .unwrap_or_else(|error| panic!("read {}: {error}", current.display()));
        for (name, _) in declared_mods(&text) {
            if let Some(file) = module_file(&current, &name) {
                if out.insert(file.clone()) {
                    frontier.push(file);
                }
            }
        }
    }
    out
}

/// Count PRODUCTION lines under `dir` for which `matches` holds, keyed by path
/// relative to it.
fn census_with(dir: &Path, matches: fn(&str) -> bool) -> (usize, BTreeMap<String, usize>) {
    let files = rust_files(dir);
    let test_files = cfg_test_module_files(dir);
    let mut hits: BTreeMap<String, usize> = BTreeMap::new();
    for path in &files {
        if test_files.contains(path) {
            continue;
        }
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let in_test = test_line_span(&text);
        let count = text
            .lines()
            .enumerate()
            .filter(|(index, _)| !in_test.contains(index))
            .filter_map(|(_, line)| code_of(line))
            .filter(|code| matches(code))
            .count();
        if count > 0 {
            let key = path
                .strip_prefix(dir)
                .expect("walked under dir")
                .to_string_lossy()
                .replace('\\', "/");
            hits.insert(key, count);
        }
    }
    (files.len(), hits)
}

/// The census proper: either spelling counts.
fn census(dir: &Path) -> (usize, BTreeMap<String, usize>) {
    census_with(dir, names_a_vendor)
}

/// The ENGINE crate. It is `zero-migrate-core`, not `zero-migrate`: this crate is the
/// COMPOSITION now, and its `src` is one file that names all three vendors on purpose.
/// Censusing THAT would be a one-file walk in which every finding is by design.
const ENGINE_CRATE: &str = "zero-migrate-core";

pub(crate) fn crate_src(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("this crate lives at <workspace>/crates/<name>")
        .join(name)
        .join("src")
}

#[test]
fn core_names_no_vendor_outside_its_recorded_exceptions() {
    let src = crate_src(ENGINE_CRATE);
    let (file_count, hits) = census(&src);

    // FLOOR ONE, part one — the ANCHORS. These answer "did the walk reach the tree at
    // all", which is the failure mode a count is too blunt to see, and they survive
    // core legitimately shrinking.
    let reached: BTreeSet<String> = rust_files(&src)
        .iter()
        .filter_map(|p| p.strip_prefix(&src).ok())
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    for anchor in WALK_ANCHORS {
        assert!(
            reached.contains(*anchor),
            "the census walked {file_count} files under {} but never reached \
             `{anchor}`, so it is not seeing the tree it claims to census. Fix the \
             walk. Do NOT delete the anchor to get green — if `{anchor}` legitimately \
             moved, point the anchor at another file that cannot move, and say which \
             in the commit.",
            src.display()
        );
    }

    // FLOOR ONE, part two — the COUNT.
    assert!(
        file_count >= SRC_FILE_FLOOR,
        "the census walked only {file_count} files under {}, below the \
         SRC_FILE_FLOOR of {SRC_FILE_FLOOR}. The anchors above PASSED, so the walk did \
         reach the tree and this is most likely core legitimately shrinking as vendor \
         code moves out — lower the floor and name the extraction that did it. If the \
         anchors FAILED, fix the walk instead and ignore this number.",
        src.display()
    );

    let allowed: BTreeMap<&str, (usize, &str)> = ALLOWED
        .iter()
        .map(|(file, count, why)| (*file, (*count, *why)))
        .collect();

    let mut problems = Vec::new();
    for (file, count) in &hits {
        match allowed.get(file.as_str()) {
            Some((limit, _)) if count == limit => {}
            Some((limit, why)) if count < limit => problems.push(format!(
                "  {file}: names a vendor {count}x in production code, the ratchet says \
                 {limit}. This is PROGRESS — lower the entry in the same commit that \
                 removed the name, so the count stays a maintained fact. The recorded \
                 reason was: {why}"
            )),
            Some((limit, why)) => problems.push(format!(
                "  {file}: names a vendor {count}x in production code, the ratchet says \
                 {limit}. The ratchet only goes DOWN. The recorded reason for the \
                 {limit} that are allowed was: {why}"
            )),
            None => problems.push(format!(
                "  {file}: names a vendor {count}x in production code and is not on the \
                 ratchet at all. Core is the NEUTRAL engine: a vendor's product name \
                 here is a fact about one backend baked into the vocabulary every \
                 backend shares. Ask the registry (a capability, a declared limit, the \
                 resolved renderer), or carry the target's own `DialectId` so the \
                 refusing backend names itself — see \
                 `IrLowerError::PartitionRelationUnsupported` and \
                 `render::backends::targets_declaring` for both shapes."
            )),
        }
    }
    for (file, (limit, why)) in &allowed {
        if !hits.contains_key(*file) {
            problems.push(format!(
                "  {file}: the ratchet says {limit} but the file names no vendor at \
                 all. If the last one is gone, DELETE the entry in the same commit — a \
                 stale allowance is an allowance nobody is reading. The recorded reason \
                 was: {why}"
            ));
        }
    }

    assert!(
        problems.is_empty(),
        "core names a vendor outside its recorded exceptions:\n{}",
        problems.join("\n")
    );
}

#[test]
fn the_needle_still_matches_where_a_vendor_name_is_the_point() {
    let src = crate_src(NEEDLE_CONTROL_CRATE);

    // EACH MATCHER SEPARATELY. One floor over the combined answer is not a control:
    // see [`names_a_vendor`] for the measurement that showed the camel matcher clearing
    // the floor on its own while every product-name needle was corrupted.
    for (label, matcher, floor) in [
        (
            "VENDOR_NEEDLES (the product names)",
            names_a_vendor_by_needle as fn(&str) -> bool,
            VENDOR_CRATE_MATCH_FLOOR,
        ),
        (
            "the `Pg` + uppercase spelling",
            names_a_vendor_by_pg_camel as fn(&str) -> bool,
            PG_CAMEL_MATCH_FLOOR,
        ),
    ] {
        let (file_count, hits) = census_with(&src, matcher);
        let total: usize = hits.values().sum();
        assert!(
            file_count > 0,
            "the needle control walked {} and found no source files at all, so it \
             proves nothing about any matcher",
            src.display()
        );
        // `names_a_vendor_by_pg_word` deliberately has no floor here, and the reason is
        // measured: the control crate carries only a handful of standalone `PG` words,
        // because it spells itself `Pg…` in types and `postgres` in ids. A density
        // floor over five lines would be noise, and lowering a floor to fit is exactly
        // what this file forbids. Its control is
        // `the_pg_word_matcher_sees_the_spelling_that_escaped_both_others` instead,
        // which pins the matcher against literals rather than against a crate's habits.
        assert!(
            total >= floor,
            "{label} found only {total} production lines across {file_count} files in \
             {NEEDLE_CONTROL_CRATE}, below its floor of {floor}. A vendor crate spells \
             its own vendor constantly in both forms, so this is that matcher having \
             gone blind — which would make the census above report a clean zero for \
             the wrong reason, and the OTHER matcher would go on covering for it. Fix \
             the matcher; do not lower this."
        );
    }
}

/// The control for the all-caps `PG` matcher, pinned against literals.
///
/// The first two assertions are the exact strings that were live in core when this
/// matcher was added, each of them an operator-facing refusal in the crate that must
/// name no vendor, and each invisible to BOTH existing matchers: the needles want
/// `pg_` with an underscore, the camel matcher wants a lowercase `g`.
///
/// The negatives matter as much. A matcher that fires on any `PG` byte pair would
/// catch these too and then catch a base64 blob, a `PGN` token and a `SPGiST` spelling,
/// and an over-firing matcher gets an allowance entry written for it — which is how a
/// census stops meaning anything.
#[test]
fn the_pg_word_matcher_sees_the_spelling_that_escaped_both_others() {
    // The fragments, not the whole strings: the originals carried `{:?}` and `{}`
    // placeholders, and clippy reads a format-shaped brace inside a literal as a
    // mistake. What is pinned is the part that escaped, which is the product name.
    for live in [
        "vendor PG primitive (op capability",
        "omit partitionBy.whenUnsupported for PG-only hash repartitioning",
        "one rebuild and no PG expand-contract",
    ] {
        assert!(
            names_a_vendor_by_pg_word(live),
            "the all-caps matcher missed {live:?}, which is a spelling that WAS live in \
             core and passed every other matcher"
        );
        assert!(
            !names_a_vendor_by_needle(live) && !names_a_vendor_by_pg_camel(live),
            "{live:?} is supposed to be the case the other two matchers cannot see; if \
             one of them now catches it, this control is measuring nothing"
        );
    }

    for benign in [
        "let png = PGN_HEADER;",
        "SPGiST is an access method",
        "const X: &str = \"aPGb\";",
        "spg_config",
    ] {
        assert!(
            !names_a_vendor_by_pg_word(benign),
            "the all-caps matcher fired on {benign:?}, where `PG` is not a word; an \
             over-firing matcher earns itself an allowance entry and the census stops \
             meaning anything"
        );
    }
}

/// The TEST-HALF splitter is itself instrumented, because it is the part that was
/// wrong by 391 lines before this file existed and a silent regression in it fails
/// toward a FALSE GREEN.
///
/// Both halves are checked against the tree rather than against a synthetic fixture,
/// so a refactor that moves the thing being measured makes this loud instead of
/// vacuous.
#[test]
fn the_test_half_splitter_is_still_seeing_the_test_half() {
    let src = crate_src(ENGINE_CRATE);

    // Half one: a `#[cfg(test)] mod x;` in a PARENT file makes the whole child file
    // test code. `differential_corpus.rs` alone held 296 vendor-naming lines and was
    // the brief's single largest reported "violation".
    let cfg_test_files = cfg_test_module_files(&src);
    let corpus = src.join("render/gen_types/differential_corpus.rs");
    assert!(
        cfg_test_files.contains(&corpus),
        "{} is declared `#[cfg(test)] mod differential_corpus;` by render/gen_types.rs \
         and holds hundreds of deliberate vendor names, but the splitter did not \
         classify it as test code. Every one of those lines is about to be counted \
         against core.",
        corpus.display()
    );
    assert!(
        cfg_test_files.len() >= 3,
        "the splitter found only {} cfg(test) module files; the tree has at least \
         three (differential_corpus, fold_projection_equality, support_matrix) and \
         finding fewer means `declared_mods` or `module_file` stopped resolving",
        cfg_test_files.len()
    );

    // Half two: in-file spans, brace-matched from EVERY `#[cfg(test)]`. `lower.rs` is
    // the case that breaks a first-attribute-wins matcher.
    let lower = std::fs::read_to_string(src.join("render/lower.rs")).expect("read lower.rs");
    let attributes = lower.match_indices("#[cfg(test)]").count();
    assert!(
        attributes >= 2,
        "render/lower.rs carries {attributes} `#[cfg(test)]` attributes; this control \
         exists because it has MORE than one and the first is not the test boundary. \
         If the file genuinely changed, point this control at another multi-attribute \
         file and say which."
    );
    let span = test_line_span(&lower);
    let total_lines = lower.lines().count();
    assert!(
        span.len() > total_lines / 4,
        "the splitter placed only {} of render/lower.rs's {total_lines} lines inside a \
         cfg(test) span. Its test module is thousands of lines long, so a small answer \
         here means the brace matcher terminated early — the exact failure that put 68 \
         test lines into the production column of the measurement this file replaced.",
        span.len()
    );
    assert!(
        span.len() < total_lines,
        "the splitter placed ALL {total_lines} lines of render/lower.rs inside a \
         cfg(test) span, which means it never found a closing brace and ran to EOF. A \
         census that classifies everything as test code reports a clean zero for the \
         worst possible reason."
    );
}
