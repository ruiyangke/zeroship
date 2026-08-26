//! The RATCHET on the owner's governing rule - *the core should be neutral, this is
//! the hard limit* - applied to the CONTRACT crate, which is the one place the rule
//! had never been measured.
//!
//! # Why this file exists, and it is not a hypothetical
//!
//! Three censuses already guard the ENGINE:
//! `dialect_matrix/core_names_no_vendor_crate.rs` (core may not reach a vendor
//! CRATE), `core_does_not_spell_a_vendors_bytes.rs` (core may not call a raw
//! spelling primitive) and `core_names_no_vendor_backend_module.rs`. Nothing guarded
//! `zero-migrate-backend`, and the cost of that gap was measured rather than guessed:
//! the brief that commissioned the neutrality pass this file lands with recorded
//! **28** code-level vendor names in this crate, taken by hand. A non-comment sweep
//! found **72**. The 44 it missed were not obscure - they included a `POSTGRES`
//! comparison deciding a security posture, a version-floor table for one server on a
//! neutral enum, and four references to a `DialectScope::PgOnly` variant that had
//! been deleted, one of them inside a message an operator could be shown.
//!
//! A hand count is a claim; this is the artifact. The rule was already written down
//! in `lib.rs` ("It deliberately holds no vendor: nothing here spells a keyword,
//! quotes an identifier or names a dialect") and being written down is exactly what
//! let it be wrong by 44.
//!
//! # What counts as NAMING a vendor here
//!
//! A vendor's product name, or an identifier prefixed with one, appearing in CODE.
//! `is_code` strips whole-line comments and the trailing half of a line comment
//! first, for the reason its siblings state at length: this crate's prose is dense
//! with vendor names ON PURPOSE - a doc that says WHICH engine a shared normal form
//! encodes, or which server answered what when a refusal was measured, is the
//! valuable part and must not be deleted to green a census. The MEASURED effect of
//! the filter on this crate is a drop from 801 raw occurrences to the handful below.
//!
//! Prose is not a name. A `#[error]` STRING is, because it is a value this crate
//! emits at whatever target reached the arm - and it is where three of the worst
//! instances lived.
//!
//! # The floors, because a scan over a DISCOVERED set fails OPEN
//!
//! Two different blindnesses share one green: narrow the WALK and it iterates
//! nothing; break the NEEDLE and it reads every file and matches nothing. So there
//! are two floors. [`SRC_FILE_FLOOR`] defends the walk. [`VENDOR_CRATE_MATCH_FLOOR`]
//! defends the needle by running the IDENTICAL matcher over a vendor crate, where
//! vendor names are the point and a zero could only mean the needle died.
//!
//! The needle control is deliberately NOT the [`ALLOWED`] entries themselves. Both of
//! those are expected to reach zero, and a census whose positive control is its own
//! remaining violations goes blind at exactly the moment it succeeds.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The vendor product names, lowercased. A line is matched against its own
/// lowercased code, so `MySql`, `MYSQL` and `mysql` are one needle.
///
/// `pg_` carries the underscore because a bare `pg` matches inside ordinary words.
/// `postgre` is not listed separately: it is a prefix of `postgres` and a line is
/// counted once however many needles hit it.
const VENDOR_NEEDLES: &[&str] = &["mysql", "sqlite", "postgres", "pgsql", "pg_", "mariadb"];

/// The ratchet: which files in this crate may name a vendor in code, and EXACTLY how
/// many times.
///
/// One more is a red, and one FEWER is also a red - a count that silently drifts down
/// is a count nobody is maintaining. A file NOT listed here may not name a vendor at
/// all. Lowering an entry is the point of this file and is expected; RAISING one, or
/// adding a file, is the thing it exists to make loud.
const ALLOWED: &[(&str, usize, &str)] = &[
    // `guard.rs` was here, at ONE, and it is off the ratchet entirely now.
    //
    // The recorded reason was `GuardConfig::for_dialect`'s explicit
    // `DialectId::new("postgres")` comparison, which reset a host-set belt-off mode to
    // `Enforced` for every other id so that a posture built for the one backend with a
    // parser could not follow a config onto a backend with no belt to skip. That
    // comparison was called a security-posture decision with no fix available from
    // here, and the fix turned out to be upstream of it: the belt-off mode itself is
    // gone, so there is no mode to reset and no id to compare against. Removing the
    // POSTURE removed the name.
    (
        "snapshot.rs",
        1,
        "`parse_nextval_sequence_ref` accepts the catalog-qualified spelling of a \
         sequence default alongside the bare one, because a catalog deparser emits \
         the qualified form when a same-signature function earlier on the search path \
         would otherwise capture it. This is a VALUE the function must READ, not a \
         name it chooses: the same function already parses `nextval(` and `::regclass` \
         and the dialect-blind differ has to recognise whatever the producer wrote. \
         Removing the one qualified alternative would not make anything neutral, it \
         would stop the differ recognising a real default — and the two spellings are \
         already covered as a pair by the shared-normal-form rule in \
         `zero-migrate/tests/dialect_matrix/backend_snapshot_privates_stay_core_only.rs`.",
    ),
];

/// The walk's floor. This crate holds 40 `.rs` files under `src`; the floor sits
/// under that with room for churn and nowhere near zero, so a walk that lost its root
/// cannot pass.
///
/// Lower it deliberately if an extraction legitimately moves files OUT, and say which
/// extraction in the commit. Never lower it to silence a walk that broke.
const SRC_FILE_FLOOR: usize = 32;

/// The NEEDLE-LIVENESS floor: matches the identical matcher must find in a crate
/// where vendor names are the whole point.
///
/// Change `VENDOR_NEEDLES` to something that no longer matches - a typo, a dropped
/// underscore, a case slip - and the walk above still visits every file, still reads
/// every line, and still reports zero violations, because it finds zero of anything.
/// A census with no positive control cannot tell "clean" from "blind".
///
/// Measured at well over this in `zero-migrate-postgres/src`, which names its own
/// vendor in nearly every file. Set low and blunt on purpose: the number only has to
/// prove the matcher is alive.
const VENDOR_CRATE_MATCH_FLOOR: usize = 40;

/// The vendor crate the needle control runs over.
const NEEDLE_CONTROL_CRATE: &str = "zero-migrate-postgres";

/// Whether a source line is CODE rather than a comment, and the code half of it.
///
/// Line-oriented on purpose, and the same filter its siblings in
/// `zero-migrate/tests/dialect_matrix/` use, with the same stated limits: it cannot
/// see inside a block comment that starts mid-line and does not try. It over-counts
/// prose into code, never the reverse, which is the safe direction for a census
/// asserting a bound.
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

/// Does this code fragment name a vendor?
///
/// Two matchers, because two spellings exist and one does not survive lowercasing.
/// The needles are matched against the lowercased fragment, which folds `MySql` and
/// `MYSQL` into one. `Pg` followed by an uppercase letter is checked on the RAW
/// fragment: `PgOnly` lowercases to `pgonly`, which contains no needle, and that
/// exact spelling is how four stale references to a deleted enum variant survived in
/// this crate until they were measured.
fn names_a_vendor(code: &str) -> bool {
    let lowered = code.to_ascii_lowercase();
    if VENDOR_NEEDLES.iter().any(|n| lowered.contains(n)) {
        return true;
    }
    let bytes = code.as_bytes();
    bytes
        .windows(3)
        .any(|w| w[0] == b'P' && w[1] == b'g' && w[2].is_ascii_uppercase())
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

/// Count the code lines naming a vendor in every file under `dir`, keyed by the path
/// relative to `dir`.
fn census(dir: &Path) -> (usize, BTreeMap<String, usize>) {
    let files = rust_files(dir);
    let mut hits: BTreeMap<String, usize> = BTreeMap::new();
    for path in &files {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let count = text
            .lines()
            .filter_map(code_of)
            .filter(|code| names_a_vendor(code))
            .count();
        if count > 0 {
            let key = path
                .strip_prefix(dir)
                .expect("walked under dir")
                .to_string_lossy()
                .into_owned();
            hits.insert(key, count);
        }
    }
    (files.len(), hits)
}

fn crate_src(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("this crate lives at <workspace>/crates/<name>")
        .join(name)
        .join("src")
}

#[test]
fn the_contract_names_no_vendor_outside_its_recorded_exceptions() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let (file_count, hits) = census(&src);

    assert!(
        file_count >= SRC_FILE_FLOOR,
        "the walk found only {file_count} source files under {}, below the \
         SRC_FILE_FLOOR of {SRC_FILE_FLOOR}. A census over a DISCOVERED set fails \
         OPEN, so this is a broken walk until proven otherwise. If an extraction \
         genuinely moved files out, lower the floor in that commit and say which.",
        src.display()
    );

    let allowed: BTreeMap<&str, (usize, &str)> = ALLOWED
        .iter()
        .map(|(file, count, why)| (*file, (*count, *why)))
        .collect();

    let mut problems = Vec::new();
    for (file, count) in &hits {
        match allowed.get(file.as_str()) {
            Some((limit, why)) if count == limit => {}
            Some((limit, why)) if count < limit => problems.push(format!(
                "  {file}: names a vendor {count}x, the ratchet says {limit}. This is \
                 PROGRESS — lower the entry in the same commit that removed the name. \
                 The recorded reason was: {why}"
            )),
            Some((limit, why)) => problems.push(format!(
                "  {file}: names a vendor {count}x, the ratchet says {limit}. The \
                 ratchet only goes DOWN. The recorded reason for the {limit} that are \
                 allowed was: {why}"
            )),
            None => problems.push(format!(
                "  {file}: names a vendor {count}x and is not on the ratchet at all. \
                 This crate is the neutral CONTRACT: it holds the traits every vendor \
                 implements, and a vendor name here is a fact about one backend baked \
                 into the vocabulary all of them share. Move it to the vendor crate \
                 that owns it, or carry the target's own DialectId so the refusing \
                 backend names itself."
            )),
        }
    }
    for (file, (limit, why)) in &allowed {
        if !hits.contains_key(*file) {
            problems.push(format!(
                "  {file}: the ratchet says {limit} but the file names no vendor at \
                 all. If the last one is gone, DELETE the entry in the same commit. \
                 The recorded reason was: {why}"
            ));
        }
    }

    assert!(
        problems.is_empty(),
        "the backend CONTRACT names a vendor outside its recorded exceptions:\n{}",
        problems.join("\n")
    );
}

#[test]
fn the_needle_still_matches_where_a_vendor_name_is_the_point() {
    let src = crate_src(NEEDLE_CONTROL_CRATE);
    let (file_count, hits) = census(&src);
    let total: usize = hits.values().sum();

    assert!(
        file_count > 0,
        "the needle control walked {} and found no source files at all, so it proves \
         nothing about the matcher",
        src.display()
    );
    assert!(
        total >= VENDOR_CRATE_MATCH_FLOOR,
        "the identical matcher found only {total} vendor-naming code lines across \
         {file_count} files in {NEEDLE_CONTROL_CRATE}, below the \
         VENDOR_CRATE_MATCH_FLOOR of {VENDOR_CRATE_MATCH_FLOOR}. A vendor crate names \
         its own vendor constantly, so this is the needle having gone blind — which \
         would make the sibling census above report a clean zero for the wrong \
         reason. Fix the needle; do not lower this."
    );
}
