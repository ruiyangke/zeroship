//! The RATCHET on the owner's governing rule — *the core should be neutral, this is
//! the hard limit* — applied to the VOCABULARY crate, the bottom of the stack.
//!
//! # Why this file exists, and why it is the last one written
//!
//! Five censuses guard the ENGINE (`zero-migrate/tests/dialect_matrix/`), and one
//! guards the CONTRACT (`zero-migrate-backend/tests/the_contract_names_no_vendor.rs`).
//! `zero-migrate-ir` had none, and it is the crate the rule matters most in: every
//! other crate in the workspace depends on it, including all three vendors, so a
//! vendor fact recorded here is a fact the other two vendors are compiled against.
//!
//! The gap was not hypothetical. What the sweep this file lands with found and fixed,
//! all seven of it, in PRODUCTION code:
//!
//! * `IrDefault::Nextval`'s doc AND its hand-written `schemars` description called the
//!   variant "a PostgreSQL `nextval('<sequence>'::regclass)` default" — a spelling, in
//!   a carrier that holds a structured `SequenceRef` and no SQL at all. The second of
//!   those two shipped in the published `ir-envelope.schema.json`.
//! * `RaiseLevel::as_sqlite_sql` handed out four SQL tokens from the neutral enum. Its
//!   only caller was the SQLite renderer; the MySQL renderer reads the SAME level,
//!   discards it, and emits `SIGNAL SQLSTATE`. The tokens moved to the backend that
//!   spells them.
//! * `FuncLanguage::Plpgsql` was one server's product name used as a WIRE TAG. The
//!   closed 2-set is the engine's own security decision — plain SQL, or the target's
//!   procedural language, nothing installed — so the set stayed and the variant became
//!   `Procedural`, with the `plpgsql` token it renders to moving to
//!   `zero-migrate-postgres`.
//! * `IrLoadError::IndefiniteTimeoutFlag`'s `#[error]` string told every author that
//!   "PostgreSQL and MySQL both read 0 as no limit" — on a gate that runs with NO
//!   target resolved, so it said that to an author aimed at a third engine too. The
//!   binding half of the same rule, one crate up, already said "the database".
//! * `code.materialized_view`'s registry docs, and the `KEY_` const's doc above them,
//!   defined a neutral capability knob as "PostgreSQL materialized views". The knob
//!   gates the op wherever a backend declares `Capability::MaterializedView`.
//!
//! # What counts as NAMING a vendor here
//!
//! A vendor's product name, or an identifier prefixed with one, in CODE. [`code_of`]
//! strips comment lines and the trailing half of a line comment first, for the reason
//! every sibling states: this tree's prose is dense with vendor names ON PURPOSE, and
//! a doc that records WHICH server was measured, or which backend populates a fact, is
//! the valuable part and must not be deleted to green a census.
//!
//! A `#[error]` string is NOT prose. Neither is a `schemars` `"description"`: both are
//! values this crate EMITS — one at whatever target reached the arm, the other into a
//! published artifact — and between them they held four of the seven above.
//!
//! # What this census can and CANNOT see
//!
//! It matches vendor product NAMES. It does not see vendor GRAMMAR, and that limit has
//! already been paid for once: a twenty-line PL/pgSQL dual-write trigger body —
//! `TG_OP`, `NEW`, `OLD`, `RETURN NEW` — sat in `zero-migrate-backend` and passed that
//! crate's identical census for its entire life there, because PL/pgSQL contains no
//! product name. Read a green here as "the vocabulary writes no vendor's NAME", never
//! as "the vocabulary holds no vendor".
//!
//! The nearest live instance of that limit is in this very crate: `TriggerStmt::Raise`
//! carries an `errcode` validated as a five-character SQLSTATE, and `IrDefault::Nextval`
//! is a shape only a sequence-having engine can satisfy. Neither spells a product and
//! neither is caught here. The GRAMMAR question is answered by different instruments —
//! `core_does_not_spell_a_vendors_bytes.rs` and the capability descriptors — not by
//! this one.
//!
//! # The TEST HALF is excluded, and that is 19 of 26 hits in this crate
//!
//! A test that pins a `DialectId`'s validity rules has to write a real dialect id, and
//! this crate CANNOT import one: all three vendor crates depend on it, so
//! `expr.rs`'s test module builds `DialectId::new("postgres")` by hand and says so in
//! a comment directly above. Excluding the test half is therefore not a convenience,
//! it is the only classification that leaves the number meaning anything: this crate
//! holds 26 vendor-naming code lines and 7 of them are production, all seven listed
//! above. The other 19 are `dialect.rs`'s validity fixtures (6), `expr.rs`'s
//! dialectal-leg tests (10) and `policy_approval.rs`'s leg tests (3).
//!
//! Two shapes make a line test code, and the splitter handles both because getting one
//! wrong cost core's census a 3.5x miscount before it was instrumented:
//! [`cfg_test_module_files`] resolves a PARENT's `#[cfg(test)] mod x;` to a whole file
//! (this crate has none today — the instrument is dormant here and must still be alive,
//! so it is controlled against a crate that does have one), and [`test_line_span`]
//! brace-matches EVERY `#[cfg(test)]` in a file with a lexer that understands raw
//! strings, char literals versus lifetimes, and block comments.
//!
//! [`the_test_half_splitter_is_still_seeing_the_test_half`] is that splitter's own
//! control, and it does not use a tuned threshold: every file in this crate carries at
//! most ONE `#[cfg(test)]`, which gives an INDEPENDENT oracle for where the test half
//! begins, and the control asserts the brace matcher agrees with it file by file.
//!
//! # The floors, because a scan over a DISCOVERED set fails OPEN
//!
//! Narrow the walk and it iterates nothing. Break a needle and it reads every line and
//! matches nothing. Both blindnesses report the same clean zero, so there are floors
//! for each: [`WALK_ANCHORS`] plus [`SRC_FILE_FLOOR`] defend the walk, and
//! [`VENDOR_NEEDLE_MATCH_FLOOR`] and [`PG_CAMEL_MATCH_FLOOR`] defend the two matchers
//! SEPARATELY, over a vendor crate where naming the vendor is the entire point.
//!
//! One floor per matcher, not one shared floor over their `||`. That is not a
//! precaution, it is a repair: core's census first asserted a single floor over the
//! combined answer, and when the product-name needles were corrupted to prove the
//! control fires, IT PASSED — `zero-migrate-postgres` is full of `PgRaw`/`PgDml`, so
//! the camel matcher cleared the floor alone while every product needle was dead.
//!
//! The needle control is deliberately NOT the [`ALLOWED`] entries. Here that is not
//! even arguable: [`ALLOWED`] is EMPTY, so a control built on it would assert nothing
//! at all.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The vendor product names, lowercased. A line is matched against its own lowercased
/// code, so `MySql`, `MYSQL` and `mysql` are one needle.
///
/// `pg_` carries the underscore because a bare `pg` matches inside ordinary words.
/// `postgre` is not listed separately: it is a prefix of `postgres`, and a line is
/// counted once however many needles hit it.
const VENDOR_NEEDLES: &[&str] = &["mysql", "sqlite", "postgres", "pgsql", "pg_", "mariadb"];

/// The ratchet: which files in this crate may name a vendor in PRODUCTION code, and
/// exactly how many times.
///
/// **It is EMPTY, and that is the whole point of the crate.** Core's equivalent holds
/// one entry at four (the registry composition, which has to name the crates it
/// composes) and the contract's holds two. This crate composes nothing, resolves
/// nothing and compares no security posture: it is the vocabulary every backend is
/// written in, so there is no shape a vendor name here can take that is not a fact
/// about one backend baked into what all of them share. Zero is the correct floor and
/// the reachable one — the sweep this file lands with reached it.
///
/// An entry added here is not automatically wrong, but it is a claim that has to be
/// argued in the same commit: the tuple carries its ARGUMENT, not just a number,
/// because a bare count tells the next reader nothing about whether it may come off.
/// One MORE than a recorded count is a red, and one FEWER is also a red — a count that
/// silently drifts down is a count nobody is maintaining.
const ALLOWED: &[(&str, usize, &str)] = &[];

/// Files the walk MUST reach — the real defence against a census that fails open.
///
/// A floor over a discovered set bounds only HOW MANY files were found; these bound
/// WHICH, and stay true at any crate size. `lib.rs` sits at the walk's root, so losing
/// it means the root is wrong. `ir.rs` is the op vocabulary and `dialect.rs` is the
/// `DialectId` newtype itself; neither can leave a crate whose entire purpose is to
/// hold them.
///
/// Pick replacements only from files that cannot move.
const WALK_ANCHORS: &[&str] = &["lib.rs", "ir.rs", "dialect.rs"];

/// The walk's COUNT floor — the weaker half of the anti-blindness pair. Check
/// [`WALK_ANCHORS`] first and trust it over this number.
///
/// Measured at 17 `.rs` files under `src` when this landed. 14 sits under that with
/// room for churn and nowhere near a walk that found nothing. Unlike core, this crate
/// is not expected to shrink — nothing is being extracted OUT of the vocabulary — so a
/// fall here is a walk to fix, not an extraction to record.
const SRC_FILE_FLOOR: usize = 14;

/// The liveness floor for the PRODUCT-NAME matcher: matches it must find in a crate
/// where naming the vendor is the whole point.
///
/// Corrupt `VENDOR_NEEDLES` — a typo, a dropped underscore, a case slip — and the walk
/// still visits every file, still reads every line, and still reports zero violations,
/// because it finds zero of anything. Measured far above this in the control crate,
/// which names its own vendor in nearly every file. Set low and blunt on purpose: the
/// number only has to prove the matcher is alive.
const VENDOR_NEEDLE_MATCH_FLOOR: usize = 40;

/// The liveness floor for the SECOND matcher, `Pg` followed by an uppercase letter.
///
/// Separate from [`VENDOR_NEEDLE_MATCH_FLOOR`] because two matchers sharing one floor
/// can satisfy it for each other — see this file's header for the run where exactly
/// that happened. Measured well above this in the control crate, which names types
/// `PgRaw`, `PgDml` and so on throughout.
const PG_CAMEL_MATCH_FLOOR: usize = 20;

/// The vendor crate every instrument's positive control runs over.
///
/// One crate serves all four because it is the only one carrying all four shapes: it
/// names its vendor constantly in both spellings, it declares a `#[cfg(test)] mod`
/// FILE ([`CFG_TEST_MODULE_FILE_CONTROL`]), and it has a file with more than one
/// `#[cfg(test)]` attribute ([`MULTI_CFG_TEST_FILE_CONTROL`]).
const NEEDLE_CONTROL_CRATE: &str = "zero-migrate-postgres";

/// The file [`cfg_test_module_files`] must classify as entirely test code in the
/// control crate: `zero-migrate-postgres/src/lib.rs` declares `#[cfg(test)] mod
/// test_fixtures;` and nothing inside the file itself says it is a test.
const CFG_TEST_MODULE_FILE_CONTROL: &str = "test_fixtures.rs";

/// The control-crate file carrying MORE THAN ONE `#[cfg(test)]`, which is the shape a
/// first-attribute-wins brace matcher gets wrong. `backend/journal_sql.rs` has two
/// (`mod tests` and `mod seam_tests`), and the span must cover both.
const MULTI_CFG_TEST_FILE_CONTROL: &str = "backend/journal_sql.rs";

/// The number of files in THIS crate the single-attribute oracle must check.
///
/// The oracle in [`the_test_half_splitter_is_still_seeing_the_test_half`] only applies
/// to a file with exactly one `#[cfg(test)]`; nine of this crate's seventeen qualify.
/// Without this floor, a scan that stopped finding attributes would check zero files
/// and pass — the same fail-open shape the walk floor defends against, one level down.
const SINGLE_ATTRIBUTE_ORACLE_FLOOR: usize = 7;

/// Whether a source line is CODE rather than a comment, and the code half of it.
///
/// Line-oriented on purpose, and the same filter its siblings use, with the same stated
/// limit: it cannot see inside a block comment that starts mid-line and does not try.
/// It over-counts prose into code, never the reverse, which is the safe direction for a
/// census asserting a bound.
fn code_of(line: &str) -> Option<&str> {
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
/// `PgOnly` lowercases to `pgonly`, which contains no needle, and that exact blind spot
/// is how four references to a DELETED `DialectScope::PgOnly` variant survived a sweep
/// of the contract crate that reported clean.
fn names_a_vendor_by_pg_camel(code: &str) -> bool {
    code.as_bytes()
        .windows(3)
        .any(|w| w[0] == b'P' && w[1] == b'g' && w[2].is_ascii_uppercase())
}

/// Does this code fragment name a vendor, by either spelling?
///
/// Kept as two separate functions rather than one `||` so each can be controlled on its
/// own. See [`the_needle_still_matches_where_a_vendor_name_is_the_point`].
fn names_a_vendor(code: &str) -> bool {
    names_a_vendor_by_needle(code) || names_a_vendor_by_pg_camel(code)
}

/// One structural token: a brace outside any string, char or comment, or the start of a
/// `#[cfg(test)]` attribute.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Token {
    Open,
    Close,
    CfgTest,
}

/// Lex `text` for [`Token`]s, skipping strings, raw strings, char literals and comments.
///
/// Hand-written rather than regex because all three of the things it must skip are
/// things a naive scan gets wrong, and each has a concrete failure in this tree:
///
/// * a raw string `r#"…{…"#` holds unbalanced braces in ordinary SQL fixtures — and
///   `expr.rs`'s test module has one carrying a whole JSON object;
/// * `'` is a lifetime far more often than a char literal here, and treating `&'a str`
///   as opening a literal swallows every brace to the next apostrophe;
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
/// Brace-matched from EVERY `#[cfg(test)]`, not the first. This crate happens to have
/// at most one per file, which is exactly why the multi-attribute case is controlled
/// against another crate: an instrument that is correct only by coincidence on the tree
/// it runs over is an instrument nobody will notice breaking.
fn test_line_span(text: &str) -> BTreeSet<usize> {
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
fn rust_files(dir: &Path) -> Vec<PathBuf> {
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
/// This crate has none today. It is here anyway because its ABSENCE is what turned
/// core's 154-line census into a 545-line one: there is nothing inside such a file that
/// says it is a test, the fact lives one file away, and the day someone moves this
/// crate's fixtures into `src/test_fixtures.rs` — the shape all three vendor crates
/// already use — every line of it would otherwise be counted against the vocabulary.
fn cfg_test_module_files(root: &Path) -> BTreeSet<PathBuf> {
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

/// Count PRODUCTION lines under `dir` for which `matches` holds, keyed by path relative
/// to it.
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

fn own_src() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn crate_src(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("this crate lives at <workspace>/crates/<name>")
        .join(name)
        .join("src")
}

#[test]
fn the_vocabulary_names_no_vendor_outside_its_recorded_exceptions() {
    let src = own_src();
    let (file_count, hits) = census(&src);

    // FLOOR ONE, part one — the ANCHORS. These answer "did the walk reach the tree at
    // all", which is the failure a count is too blunt to see.
    let reached: BTreeSet<String> = rust_files(&src)
        .iter()
        .filter_map(|p| p.strip_prefix(&src).ok())
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    for anchor in WALK_ANCHORS {
        assert!(
            reached.contains(*anchor),
            "the census walked {file_count} files under {} but never reached `{anchor}`, \
             so it is not seeing the tree it claims to census. Fix the walk. Do NOT \
             delete the anchor to get green — if `{anchor}` legitimately moved, point \
             the anchor at another file that cannot move, and say which in the commit.",
            src.display()
        );
    }

    // FLOOR ONE, part two — the COUNT.
    assert!(
        file_count >= SRC_FILE_FLOOR,
        "the census walked only {file_count} files under {}, below the SRC_FILE_FLOOR \
         of {SRC_FILE_FLOOR}. The anchors above PASSED, so the walk did reach the tree — \
         but this crate is the vocabulary and is not being extracted FROM, so a real \
         fall here still wants an explanation in the commit that lowers the floor.",
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
                "  {file}: names a vendor {count}x in production code, and ALLOWED is \
                 empty — this crate names no vendor anywhere, which is the property \
                 every other crate depends on it for. Every backend is written in this \
                 vocabulary, so a vendor's name here is one backend's fact compiled into \
                 all of them. The three landed answers, in the order to try them: the \
                 thing is misnamed and does something neutral (`is_valid_pg_type_ref` -> \
                 `is_conservative_type_ref`); it is a refusal, and should name its target \
                 from the carried `DialectId` rather than a compiled-in string \
                 (`VendorPgOnly` -> `VendorUnsupported {{ op_kind, dialect }}`); or it is \
                 genuinely one backend's, and belongs in that backend's crate \
                 (`MysqlPhysicalType`, and `RaiseLevel`'s SQL tokens)."
            )),
        }
    }
    for (file, (limit, why)) in &allowed {
        if !hits.contains_key(*file) {
            problems.push(format!(
                "  {file}: the ratchet says {limit} but the file names no vendor at all. \
                 If the last one is gone, DELETE the entry in the same commit — a stale \
                 allowance is an allowance nobody is reading. The recorded reason was: \
                 {why}"
            ));
        }
    }

    assert!(
        problems.is_empty(),
        "the neutral VOCABULARY names a vendor in production code:\n{}",
        problems.join("\n")
    );
}

#[test]
fn the_needle_still_matches_where_a_vendor_name_is_the_point() {
    let src = crate_src(NEEDLE_CONTROL_CRATE);

    // EACH MATCHER SEPARATELY. One floor over the combined answer is not a control over
    // either — see [`names_a_vendor`] and this file's header.
    for (label, matcher, floor) in [
        (
            "VENDOR_NEEDLES (the product names)",
            names_a_vendor_by_needle as fn(&str) -> bool,
            VENDOR_NEEDLE_MATCH_FLOOR,
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
            "the needle control walked {} and found no source files at all, so it proves \
             nothing about any matcher",
            src.display()
        );
        assert!(
            total >= floor,
            "{label} found only {total} production lines across {file_count} files in \
             {NEEDLE_CONTROL_CRATE}, below its floor of {floor}. A vendor crate spells \
             its own vendor constantly in both forms, so this is that matcher having \
             gone blind — which would make the census above report a clean zero for the \
             wrong reason, and the OTHER matcher would go on covering for it. Fix the \
             matcher; do not lower this."
        );
    }
}

/// The TEST-HALF splitter is instrumented, because a silent regression in it fails
/// toward a FALSE GREEN: a splitter that swallows whole files reports a vocabulary with
/// no vendor names in it for the worst possible reason.
///
/// Three checks, one per way the splitter can be wrong, and none of them a tuned
/// threshold.
#[test]
fn the_test_half_splitter_is_still_seeing_the_test_half() {
    let src = own_src();

    // ── Half one: in-file spans, against an INDEPENDENT oracle ──────────────────────
    //
    // Every file in this crate carries at most ONE `#[cfg(test)]`, so where the test
    // half begins is knowable without the brace matcher: it is the line the attribute
    // is on. The matcher must agree, file by file, and its span must be contiguous and
    // must never be the whole file.
    let mut checked = 0usize;
    for path in rust_files(&src) {
        let text = std::fs::read_to_string(&path).expect("read source");
        if text.matches("#[cfg(test)]").count() != 1 {
            continue;
        }
        let attribute_line = text
            .lines()
            .position(|line| line.trim() == "#[cfg(test)]")
            .expect("the single attribute sits on its own line in this crate");
        let span = test_line_span(&text);
        let total = text.lines().count();
        let name = path
            .file_name()
            .expect("file")
            .to_string_lossy()
            .to_string();

        let first = *span.iter().next().unwrap_or_else(|| {
            panic!(
                "{name} carries a `#[cfg(test)]` on line {} but the splitter placed NO \
                 line inside a test span. Every vendor name in that module is about to \
                 be counted against the vocabulary.",
                attribute_line + 1
            )
        });
        assert_eq!(
            first,
            attribute_line,
            "{name}: the splitter's test span starts at line {} but the file's only \
             `#[cfg(test)]` is on line {}. The brace matcher and the attribute disagree, \
             so one of them is wrong and the census is splitting production from test on \
             the wrong boundary.",
            first + 1,
            attribute_line + 1
        );
        let last = *span.iter().next_back().expect("non-empty, checked above");
        assert_eq!(
            span.len(),
            last - first + 1,
            "{name}: the splitter's test span covers {} lines but runs from {} to {}, so \
             it is not contiguous. A `#[cfg(test)]` item is one brace-balanced range; \
             holes in it mean the lexer lost track inside a string or a comment.",
            span.len(),
            first + 1,
            last + 1
        );
        assert!(
            span.len() < total,
            "{name}: the splitter placed ALL {total} lines inside a test span, which \
             means it never found a closing brace and ran to EOF. A census that \
             classifies everything as test code reports a clean zero for the worst \
             possible reason."
        );
        checked += 1;
    }
    assert!(
        checked >= SINGLE_ATTRIBUTE_ORACLE_FLOOR,
        "the single-attribute oracle checked only {checked} files, below the \
         SINGLE_ATTRIBUTE_ORACLE_FLOOR of {SINGLE_ATTRIBUTE_ORACLE_FLOOR}. It applies to \
         a file with exactly one `#[cfg(test)]`, and this crate has nine — so finding \
         fewer means the attribute scan or the walk stopped working, and this control \
         was about to pass by checking nothing."
    );

    // ── Half two: MORE THAN ONE `#[cfg(test)]` in a file ────────────────────────────
    //
    // The shape a first-attribute-wins matcher gets wrong, and the one that put 68 test
    // lines into core's production column. This crate has no instance, so the control
    // runs over the crate that does.
    let multi = crate_src(NEEDLE_CONTROL_CRATE).join(MULTI_CFG_TEST_FILE_CONTROL);
    let text = std::fs::read_to_string(&multi)
        .unwrap_or_else(|error| panic!("read {}: {error}", multi.display()));
    let attributes = text.match_indices("#[cfg(test)]").count();
    assert!(
        attributes >= 2,
        "{MULTI_CFG_TEST_FILE_CONTROL} carries {attributes} `#[cfg(test)]` attributes; \
         this control exists because it has MORE than one and the first is not the test \
         boundary. If the file genuinely changed, point the control at another \
         multi-attribute file and say which in the commit."
    );
    let last_attribute = text
        .lines()
        .enumerate()
        .filter(|(_, line)| line.trim() == "#[cfg(test)]")
        .map(|(index, _)| index)
        .last()
        .expect("at least two, asserted above");
    let span = test_line_span(&text);
    assert!(
        span.contains(&last_attribute),
        "the splitter's test span for {MULTI_CFG_TEST_FILE_CONTROL} does not cover line \
         {}, where the LAST of its {attributes} `#[cfg(test)]` attributes sits. A matcher \
         that stops after the first attribute's closing brace reports every later test \
         module as production code.",
        last_attribute + 1
    );

    // ── Half three: a whole FILE made test code by its parent's `mod` ───────────────
    //
    // Dormant in this crate (it has no such declaration) and controlled anyway, because
    // an instrument nobody exercises is an instrument nobody notices breaking.
    let control_src = crate_src(NEEDLE_CONTROL_CRATE);
    let cfg_test_files = cfg_test_module_files(&control_src);
    let expected = control_src.join(CFG_TEST_MODULE_FILE_CONTROL);
    assert!(
        cfg_test_files.contains(&expected),
        "{} is declared `#[cfg(test)] mod` by {NEEDLE_CONTROL_CRATE}'s lib.rs, and \
         nothing inside the file says it is a test — but the splitter did not classify \
         it as one. `declared_mods` or `module_file` has stopped resolving, and the day \
         this crate gains such a file every line of it will be counted against the \
         vocabulary.",
        expected.display()
    );
}
