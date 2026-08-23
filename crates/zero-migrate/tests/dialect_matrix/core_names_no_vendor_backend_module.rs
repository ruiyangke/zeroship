//! The RATCHET on the SECOND way core can resolve a vendor: by naming the vendor's
//! BACKEND MODULE.
//!
//! Its sibling [`core_names_no_vendor_crate`](super::core_names_no_vendor_crate)
//! measures core reaching a vendor CRATE (`zero_migrate_postgres`). That census was
//! green while a strictly larger coupling ran underneath it, because the three
//! shipping backends also have an IN-CRATE home — `apply::backend::{postgres,
//! mysql, sqlite}` — and a path through *that* names no crate at all.
//!
//! What the in-crate spelling was hiding, measured at `dd6809d1`:
//!
//! | file | names a vendor backend module | what it was |
//! |------|------------------------------|-------------|
//! | `ops/status.rs` | 7 | the neutral `status`/`history` verbs read PostgreSQL's journal |
//! | `apply/baseline.rs` | 3 | the neutral baseline body wrote PostgreSQL's journal |
//! | `lib.rs` | 3 | the crate root re-exported 16 PostgreSQL SQL items as neutral API |
//!
//! Core's generic journal WAS PostgreSQL's journal, and the crate's public surface
//! PROMISED one vendor's implementation. Neither is visible to a crate-name census.
//!
//! # What counts as naming a vendor backend module
//!
//! A `postgres::` / `mysql::` / `sqlite::` segment in PATH position — preceded by a
//! character that cannot continue an identifier. That is what separates
//! `crate::apply::backend::postgres::journal_sql::applied` (core reached PostgreSQL)
//! from `zero_migrate_postgres::VENDOR` (the sibling census's subject, whose
//! `postgres::` is preceded by `_`) and from prose about either.
//!
//! The three vendors' OWN subtrees are not walked. A file under
//! `apply/backend/postgres/` naming `postgres` is that vendor describing itself,
//! which is the one place the knowledge belongs.
//!
//! # The allowances
//!
//! [`ALLOWED`] records, per file, EXACTLY how many times it may name a vendor
//! backend module. One more is a red and one FEWER is also a red, because a count
//! that drifts down silently is a count nobody maintains. A file not listed may not
//! name one at all.
//!
//! # The floors, because a scan over a DISCOVERED set fails OPEN
//!
//! Narrow the walk and it iterates nothing and reports clean; break the needle and
//! it reads every file and still reports clean. Two different blindnesses with the
//! same green, so there are two floors: [`WALKED_FILE_FLOOR`] for the walk, and the
//! `render/dml.rs` entry for the needle — a positive control that must keep
//! matching exactly twice.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The three vendor backend module segments, as they appear in a Rust path.
const VENDOR_MODULES: &[&str] = &["postgres::", "mysql::", "sqlite::"];

/// The vendors' own subtrees, relative to `crates/zero-migrate/src`. A vendor
/// naming itself inside its own module is not core resolving a vendor.
///
/// TWO now, not three: `apply/backend/mysql/` left the engine entirely for
/// `zero-migrate-mysql`, so an exemption for it would be an exemption for nothing —
/// and a stale exemption is the shape that quietly stops covering a directory
/// somebody later recreates under that path.
const VENDOR_SUBTREES: &[&str] = &["apply/backend/postgres/", "apply/backend/sqlite/"];

/// The ratchet: which core files may name a vendor backend module, and how often.
///
/// Paths are relative to `crates/zero-migrate/src`. Lowering an entry is the point
/// of this file. RAISING one, or adding a file, is what it exists to make loud.
const ALLOWED: &[(&str, usize)] = &[
    // The backend composition root: it declares the vendor submodules still in this
    // crate and re-exports their backend TYPES, which is how a host names the engine
    // it wants. Nothing here reaches a vendor's SQL.
    //
    // TWO, down from three, and the third did not move — it went away. MySQL's
    // execution half is `zero-migrate-mysql` now, and this file does NOT re-export
    // it: a `pub use zero_migrate_mysql::MysqlBackend` here would be core naming a
    // vendor CRATE outside the registry, which is what the sibling census
    // `core_names_no_vendor_crate` forbids. Closing one coupling by opening the
    // other would have been a wash. A host that wants `MysqlBackend` names the
    // vendor crate; `zero-migrate-node`'s bridge does exactly that.
    //
    // The remaining two go the same way when `postgres` and `sqlite` follow, which
    // is why this entry is no longer marked PERMANENT.
    ("apply/backend/mod.rs", 2),
    // The cross-seam identifier-quoting test. `all_engine_seams_render_uniformly`
    // asserts that the author seam and the PostgreSQL journal seam quote and
    // fail-closed BYTE-IDENTICALLY, which it cannot do without naming both. A
    // `#[cfg(test)]` seam, not a production path, and also this census's positive
    // control (see the module header).
    ("render/dml.rs", 2),
];

/// The walk's floor. `crates/zero-migrate/src` holds 48 `.rs` files outside the
/// remaining vendor subtrees; the floor sits under that with room for ordinary churn
/// but nowhere near zero, so a walk that lost its root cannot pass.
///
/// Raise it deliberately if the crate grows. NEVER lower it to get green — a drop
/// means the walk stopped seeing files, which is the failure this defends.
const WALKED_FILE_FLOOR: usize = 45;

/// Whether a source line is CODE rather than a comment.
///
/// Line-oriented on purpose, and its limits are stated rather than papered over: a
/// `//`, `///` or `//!` line is prose, a line whose first non-space character is `*`
/// is the continuation of a block comment, and anything else is code. A vendor
/// module named inside a trailing `// …` on a code line therefore still counts, and
/// that is the safe direction — this census over-counts prose into code, never the
/// reverse. Core is dense with prose naming these modules (the drift and engine
/// modules alone spend dozens of lines explaining which coupling they removed), so
/// without this filter the census would be red on day one for the wrong reason.
fn is_code(line: &str) -> bool {
    let t = line.trim_start();
    !(t.starts_with("//") || t.starts_with('*') || t.starts_with("/*"))
}

/// How many times `line` names a vendor backend module in PATH position.
///
/// A match must not be preceded by a character that can continue a Rust identifier,
/// which is what keeps `zero_migrate_postgres::VENDOR` (preceded by `_`) and
/// `MyPostgres::new` out of this census and inside its sibling's.
fn vendor_module_names(line: &str) -> usize {
    let bytes = line.as_bytes();
    let mut n = 0;
    for needle in VENDOR_MODULES {
        let mut from = 0;
        while let Some(offset) = line[from..].find(needle) {
            let at = from + offset;
            let preceded_by_ident = at > 0 && {
                let prev = bytes[at - 1];
                prev.is_ascii_alphanumeric() || prev == b'_'
            };
            if !preceded_by_ident {
                n += 1;
            }
            from = at + needle.len();
        }
    }
    n
}

/// Every `.rs` file under `root` outside the vendors' own subtrees, sorted, as
/// paths relative to `root`.
fn walked_files(root: &Path) -> Vec<(String, PathBuf)> {
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
                let rel = path
                    .strip_prefix(root)
                    .expect("walked from root")
                    .to_string_lossy()
                    .replace('\\', "/");
                if !VENDOR_SUBTREES.iter().any(|v| rel.starts_with(v)) {
                    out.push((rel, path));
                }
            }
        }
    }
    out.sort();
    out
}

/// The census.
#[test]
fn core_names_no_vendor_backend_module_outside_the_composition_root() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = walked_files(&src);

    // FLOOR ONE — the WALK. A scan over a discovered set fails open.
    assert!(
        files.len() >= WALKED_FILE_FLOOR,
        "the census walked only {} files under {} (excluding the vendor subtrees), \
         below the floor of {WALKED_FILE_FLOOR}. A narrowed walk finds nothing and \
         reports clean; fix the walk, do not lower the floor to get green.",
        files.len(),
        src.display()
    );

    let mut found: BTreeMap<String, usize> = BTreeMap::new();
    for (rel, path) in &files {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let n: usize = text
            .lines()
            .filter(|l| is_code(l))
            .map(vendor_module_names)
            .sum();
        if n > 0 {
            found.insert(rel.clone(), n);
        }
    }

    // FLOOR TWO — the NEEDLE. `render/dml.rs` names the PostgreSQL journal seam
    // exactly twice, in a test that exists BECAUSE it must name it. It is a
    // positive control: if this stops matching, the census has gone blind and every
    // other zero below it is meaningless.
    let control = found.get("render/dml.rs").copied().unwrap_or(0);
    assert_eq!(
        control, 2,
        "the needle found {control} vendor-backend-module names in render/dml.rs, \
         expected 2 (the two `journal_sql::quote_ident_for_test` calls in \
         `all_engine_seams_render_uniformly`). Either that test was restructured — \
         update this control deliberately — or the needle stopped matching, in which \
         case the whole census is blind."
    );

    let allowed: BTreeMap<&str, usize> = ALLOWED.iter().copied().collect();
    let mut violations: Vec<String> = Vec::new();
    for (file, count) in &found {
        match allowed.get(file.as_str()) {
            None => violations.push(format!(
                "  {file}: names a vendor backend module {count}x but is NOT on the \
                 ratchet. Core asks the registered backend through \
                 `MigrationBackend`; it does not reach a vendor's module by name."
            )),
            Some(&limit) if *count > limit => violations.push(format!(
                "  {file}: names a vendor backend module {count}x, ratchet says \
                 {limit}. The ratchet only goes DOWN."
            )),
            Some(&limit) if *count < limit => violations.push(format!(
                "  {file}: names a vendor backend module {count}x, ratchet says \
                 {limit}. This is progress — lower the ratchet in the same commit \
                 that made it true, so the count stays a maintained fact."
            )),
            Some(_) => {}
        }
    }
    for (file, _) in ALLOWED {
        if !found.contains_key(*file) {
            violations.push(format!(
                "  {file}: on the ratchet but names no vendor backend module. Remove \
                 its entry in the commit that closed it — a stale allowance is an \
                 allowance nobody is reading."
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "core names a vendor backend module outside the composition root:\n{}",
        violations.join("\n")
    );
}
