//! The RATCHET on the owner's governing rule: *the core should be neutral, this is
//! the hard limit*. Core must not NAME, CARRY or RESOLVE a vendor — and this file
//! measures the first of the three, because it is the one a grep can see.
//!
//! # What "name a vendor" means here, precisely
//!
//! The three shipping backends are their own crates. `zero-migrate` therefore has
//! three `[dependencies]` lines it cannot avoid, and three crate idents —
//! `zero_migrate_postgres`, `zero_migrate_sqlite`, `zero_migrate_mysql` — that any
//! module in core is free to write. Every place core writes one is a place where the
//! engine has resolved a vendor WITHOUT going through the registry, and the registry
//! is the whole point of the crate split: it is what makes a vendor deletable and a
//! fourth backend a `[dependencies]` line plus one entry.
//!
//! So the target is not zero. It is ONE FILE: `render/backends/mod.rs`, which IS the
//! registry composition. Three lines there — the three entries in the `SHIPPING`
//! array — are the engine naming its vendors once, on purpose, in the place designed
//! to hold that knowledge. Dispatch itself is now a `VendorSet` lookup by open id.
//!
//! # Why a ratchet and not an assertion of the target
//!
//! Because a permanently-red test is not a guard, it is noise that trains people to
//! ignore a colour. The tree had FOUR such files when this census was written, and
//! they came off one commit at a time:
//!
//! | file | lines | what it was | closed by |
//! |------|-------|-------------|-----------|
//! | `render/backends/mod.rs` | 3 | the registry — the PERMANENT entry | never |
//! | `lib.rs` | 3 | `pub use {Mysql,Pg,Sqlite}Guard` at the crate root | closed |
//! | `render/vendor.rs` | 1 | `pub use zero_migrate_postgres::render_vendor_op` | closed |
//! | `render/declarative.rs` | 1 | `use zero_migrate_mysql::collation::{…}` | closed |
//!
//! `render/vendor.rs` closed by putting the vendor-op surface behind
//! `DmlRenderer::render_vendor_op`. That one is now ALSO a privacy rule — `mod vendor`
//! is private in `zero-migrate-postgres` and the crate-root `pub use` is gone, so
//! naming it is an E0603 rather than a finding here. Where a rule can be a privacy it
//! should be; this census is the backstop for the ones that cannot, which is why its
//! entry came off rather than being kept as a duplicate. The registry entry fell
//! from six to three when its exhaustive enum match became a `VendorSet` lookup.
//!
//! `render/declarative.rs` closed when collation pinning and stripping became two
//! required primitive methods on `SchemaRenderer`. The stripping body moved
//! verbatim into the MySQL backend; core now asks the resolved renderer and names no
//! vendor crate.
//!
//! [`ALLOWED`] is that table. A file listed there names a vendor crate EXACTLY the
//! recorded number of times: one more is a red, and one FEWER is also a red, because
//! a count that silently drifts down is a count nobody is maintaining. A file NOT
//! listed there may not name a vendor crate at all.
//!
//! Lowering an entry is the point of the file and is expected. RAISING one, or adding
//! a file, is the thing this exists to make loud.
//!
//! # Mentions are not names, and that distinction is LIVE in this tree
//!
//! `crates/zero-migrate/src` is dense with prose about the vendor crates — the
//! registry module alone spends eighty lines explaining which coupling it removed and
//! which it could not. A census that counted every occurrence would be red on day one
//! for exactly the wrong reason, and the only way to green it would be deleting the
//! documentation that states the rule. The sibling census
//! `core_does_not_spell_a_vendors_bytes.rs` records the same hazard against
//! `ansi_double_quote_ident`, which core mentions six times, every one a doc comment
//! saying why it must not be called.
//!
//! So [`is_code`] strips comment lines before matching. It is a line-oriented filter
//! rather than a Rust parser, which is the honest description of what it can do: it
//! cannot see inside a block comment that starts mid-line, and it does not try. The
//! MEASURED effect on this tree is that the census drops from 60-odd occurrences to
//! the 11 that are real code.
//!
//! # The floors, because a scan over a DISCOVERED set fails OPEN
//!
//! Narrow the walk and it iterates nothing, finds nothing, reports clean. Break the
//! needle and it reads every file and still finds nothing. Those are two different
//! blindnesses with the same green, so there are two floors: [`SRC_FILE_FLOOR`] for
//! the walk and the `render/backends/mod.rs` entry in [`ALLOWED`] for the needle —
//! the registry's three lines are a positive control that must keep matching.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The three vendor crate idents, as they appear in Rust code.
///
/// The underscore spelling is the only one that can be a PATH. A hyphenated
/// `zero-migrate-postgres` can only ever be prose or a `Cargo.toml` key, so matching
/// the underscore form is what separates "core reached this crate" from "core talked
/// about it".
const VENDOR_CRATES: &[&str] = &[
    "zero_migrate_mysql",
    "zero_migrate_postgres",
    "zero_migrate_sqlite",
];

/// The ratchet: which core files may name a vendor crate, and exactly how many times.
///
/// Paths are relative to `crates/zero-migrate/src`. See the module header for what
/// each entry is and which of them is permanent.
const ALLOWED: &[(&str, usize)] = &[
    // PERMANENT. The registry composition's three `SHIPPING` entries. Dispatch is a
    // `VendorSet` lookup and names no vendor crate itself.
    ("render/backends/mod.rs", 3),
];

/// The walk's floor. `crates/zero-migrate/src` held 87 `.rs` files when this was
/// written; the floor sits under that with room for ordinary churn but nowhere near
/// zero, so a walk that lost its root cannot pass.
///
/// Raise it deliberately if the crate grows a lot. NEVER lower it to get green — a
/// drop means the walk stopped seeing files, which is the failure this defends.
const SRC_FILE_FLOOR: usize = 70;

/// Whether a source line is CODE rather than a comment.
///
/// Line-oriented on purpose, and its limits are stated rather than papered over: a
/// `//` or `//!` or `///` line is prose, a line whose first non-space character is
/// `*` is the continuation of a block comment, and anything else is treated as code.
/// A vendor ident inside a trailing `// …` on a code line therefore still counts, and
/// that is the safe direction — this census over-counts prose into code, never the
/// reverse.
fn is_code(line: &str) -> bool {
    let t = line.trim_start();
    !(t.starts_with("//") || t.starts_with('*') || t.starts_with("/*"))
}

/// Every `.rs` file under `root`, workspace-relative to `root` itself, sorted.
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

/// The census.
#[test]
fn core_names_no_vendor_crate_outside_the_registry() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = src_files(&src);

    // FLOOR ONE — the WALK. A scan over a discovered set fails open.
    assert!(
        files.len() >= SRC_FILE_FLOOR,
        "the census walked only {} files under {}, below the floor of {SRC_FILE_FLOOR}. \
         A narrowed walk finds nothing and reports clean; fix the walk, do not lower \
         the floor to get green.",
        files.len(),
        src.display()
    );

    let mut found: BTreeMap<String, usize> = BTreeMap::new();
    for path in &files {
        let rel = path
            .strip_prefix(&src)
            .expect("walked from src")
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let n = text
            .lines()
            .filter(|l| is_code(l))
            .filter(|l| VENDOR_CRATES.iter().any(|c| l.contains(c)))
            .count();
        if n > 0 {
            found.insert(rel, n);
        }
    }

    let allowed: BTreeMap<&str, usize> = ALLOWED.iter().copied().collect();

    // FLOOR TWO — the NEEDLE. The registry is a positive control: it names all three
    // vendor crates once each, and it is not going to stop. If this stops matching,
    // the census has gone blind and every other zero below it is meaningless.
    let registry = found.get("render/backends/mod.rs").copied().unwrap_or(0);
    assert_eq!(
        registry, 3,
        "the needle found {registry} vendor-crate names in render/backends/mod.rs, \
         expected 3 (the three `SHIPPING` entries). Either the \
         registry was restructured — update this control deliberately — or \
         `VENDOR_CRATES` stopped matching, in which case the whole census is blind."
    );

    let mut violations: Vec<String> = Vec::new();
    for (file, count) in &found {
        match allowed.get(file.as_str()) {
            None => violations.push(format!(
                "  {file}: names a vendor crate {count}x but is NOT on the ratchet. \
                 Core resolves a vendor through `render::backends`, not by crate name."
            )),
            Some(&limit) if *count > limit => violations.push(format!(
                "  {file}: names a vendor crate {count}x, ratchet says {limit}. \
                 The ratchet only goes DOWN."
            )),
            Some(&limit) if *count < limit => violations.push(format!(
                "  {file}: names a vendor crate {count}x, ratchet says {limit}. \
                 This is progress — lower the ratchet in the same commit that made it \
                 true, so the count stays a maintained fact."
            )),
            Some(_) => {}
        }
    }
    for (file, _) in ALLOWED {
        if !found.contains_key(*file) {
            violations.push(format!(
                "  {file}: on the ratchet but names no vendor crate. Remove its entry \
                 in the commit that closed it — a stale allowance is an allowance \
                 nobody is reading."
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "core names a vendor crate outside the registry:\n{}",
        violations.join("\n")
    );
}
