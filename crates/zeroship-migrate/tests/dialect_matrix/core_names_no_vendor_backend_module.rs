//! The RATCHET on the SECOND way core can resolve a vendor: by naming the vendor's
//! BACKEND MODULE.
//!
//! Its sibling [`core_names_no_vendor_crate`](super::core_names_no_vendor_crate)
//! measures core reaching a vendor CRATE (`zeroship_migrate_postgres`). That census was
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
//! `crate::zeroship_migrate_postgres::backend::journal_sql::applied` (core reached PostgreSQL)
//! from `zeroship_migrate_postgres::VENDOR` (the sibling census's subject, whose
//! `postgres::` is preceded by `_`) and from prose about either.
//!
//! # The ratchet reached zero
//!
//! It is an EMPTY [`ALLOWED`] now, and an empty [`VENDOR_SUBTREES`] with it. Every
//! vendor execution half has left `crates/zero-migrate-core/src` — MySQL at `6a7dc142`,
//! SQLite at `11772209`, PostgreSQL here — so there is no vendor subtree to exempt,
//! no composition root declaring a vendor submodule, and no file in core that names
//! one. The ratchet only ever went DOWN, and this is the bottom.
//!
//! # The floors, because a scan over a DISCOVERED set fails OPEN
//!
//! Narrow the walk and it iterates nothing and reports clean; break the needle and
//! it reads every file and still reports clean. Two different blindnesses with the
//! same green, so there are two floors: [`WALKED_FILE_FLOOR`] for the walk, and
//! [`needle_positive_control`] for the needle.
//!
//! The needle's control USED TO BE a corpus hit — `render/dml.rs` named the
//! PostgreSQL journal seam exactly twice, in `all_engine_seams_render_uniformly`,
//! which had to name it to compare it. That leg went with the execution half (it is
//! `zeroship_migrate_postgres::backend::journal_sql`'s
//! `the_journal_seam_renders_uniformly_and_fails_closed` now), so the corpus count is
//! a true zero and a corpus control is no longer available AT ALL: there is nothing
//! left in core for it to match. The control is a FIXTURE instead — the identical
//! matcher over lines whose answers are stated here — which proves the needle still
//! discriminates without requiring core to keep a violation alive to be measured by.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The three vendor backend module segments, as they appear in a Rust path.
const VENDOR_MODULES: &[&str] = &["postgres::", "mysql::", "sqlite::"];

/// The vendors' own subtrees, relative to `crates/zero-migrate-core/src`. A vendor
/// naming itself inside its own module is not core resolving a vendor.
///
/// NONE now. `apply/backend/mysql/`, then `apply/backend/sqlite/`, then
/// `apply/backend/postgres/` left the engine entirely, for
/// `zero-migrate-{mysql,sqlite,postgres}/src/backend/`, so an exemption for any of
/// them would be an exemption for nothing — and a stale exemption is the shape that
/// quietly stops covering a directory somebody later recreates under that path. The
/// walk reads ALL of `src` now, which is strictly more than it read before.
const VENDOR_SUBTREES: &[&str] = &[];

/// The ratchet: which core files may name a vendor backend module, and how often.
///
/// Paths are relative to `crates/zero-migrate-core/src`. Lowering an entry is the point of
/// this file. RAISING one, or adding a file, is what it exists to make loud.
///
/// EMPTY, and both entries that went are gone rather than moved:
///
/// - `apply/backend/mod.rs` declared `pub mod postgres` and re-exported
///   `PostgresBackend`. The module left for `zero-migrate-postgres` and the re-export
///   was NOT repointed: a `pub use zeroship_migrate_postgres::PostgresBackend` here would
///   be core naming a vendor CRATE outside the registry, which is what the sibling
///   census `core_names_no_vendor_crate` forbids. Closing one coupling by opening the
///   other would have been a wash. A host that wants `PostgresBackend` names the
///   vendor crate; `zero-migrate-node`'s bridge already does that for all three.
/// - `render/dml.rs`'s two were `all_engine_seams_render_uniformly` comparing the
///   author seam against the PostgreSQL journal seam. That leg went WITH the
///   execution half, exactly as the `role` leg did before it; the engine's test keeps
///   the seam it can still see.
const ALLOWED: &[(&str, usize)] = &[];

/// The walk's floor. `crates/zero-migrate-core/src` holds 48 `.rs` files; the floor sits
/// under that with room for ordinary churn but nowhere near zero, so a walk that lost
/// its root cannot pass.
///
/// Unchanged by the SQLite extraction and unchanged by the PostgreSQL one, and that
/// is the point of measuring OUTSIDE the vendor subtrees: the files that left
/// `apply/backend/{sqlite,postgres}/` were already excluded from this count, so it
/// reads 48 before and 48 after both. The sibling censuses that walk ALL of `src` had
/// to be lowered twice; this one did not move.
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
/// which is what keeps `zeroship_migrate_postgres::VENDOR` (preceded by `_`) out of this
/// census and inside its sibling's.
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

/// FLOOR TWO — the NEEDLE, as a fixture.
///
/// The corpus count is a real zero: no file in `crates/zero-migrate-core/src` names a
/// vendor backend module any more. So the control cannot be a corpus hit without
/// core keeping a violation alive purely to be measured by, which would be the
/// census demanding the defect it forbids. It runs the identical matcher over lines
/// whose answers are STATED, which is what makes a broken needle loud:
///
/// * the three vendor module names in path position each count once — a needle that
///   stopped matching them returns 0 here and this fails;
/// * `zeroship_migrate_postgres::VENDOR` counts ZERO, because its `postgres::` is
///   preceded by `_`. That discount is the whole boundary between this census and
///   its crate-naming sibling, so a needle that lost it returns 1 here and this
///   fails just as loudly. Both directions, not just the blind one.
fn needle_positive_control() {
    let hits = vendor_module_names("use crate::apply::backend::postgres::journal_sql::applied;");
    assert_eq!(
        hits, 1,
        "the needle found {hits} vendor backend modules in a line that carries exactly \
         one. It has stopped matching, and every zero the census reports is blind."
    );
    let all = vendor_module_names("postgres::a mysql::b sqlite::c");
    assert_eq!(
        all, 3,
        "the needle found {all} of the three vendor backend modules in a line that \
         carries all three; it recognizes some vendors and not others."
    );
    let crate_named = vendor_module_names("pub use zeroship_migrate_postgres::VENDOR;");
    assert_eq!(
        crate_named, 0,
        "the needle counted {crate_named} vendor backend modules in a line that names \
         a vendor CRATE and no module. That is the sibling census's subject, not this \
         one's; without the leading-identifier discount this file would red on every \
         registry line in core."
    );
}

/// The ENGINE's source root, which is `zero-migrate-core/src` and no longer this
/// crate's own `src`.
///
/// This crate is the COMPOSITION now: its `src` is one file that names the three
/// vendors on purpose. Walking it would give this census a one-file tree in which
/// every finding is by design — a green that means nothing. The floors below caught
/// exactly that when the split landed, which is why they exist.
fn engine_src() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("this crate lives at <workspace>/crates/<name>")
        .join("zero-migrate-core")
        .join("src")
}

/// The census.
#[test]
fn core_names_no_vendor_backend_module_outside_the_composition_root() {
    let src = engine_src();
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

    // FLOOR TWO — the NEEDLE, run over a fixture rather than over the corpus,
    // because the corpus is a true zero now and has nothing left to control on.
    needle_positive_control();

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
