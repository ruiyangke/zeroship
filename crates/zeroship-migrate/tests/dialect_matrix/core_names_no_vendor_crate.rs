//! The RATCHET on the owner's governing rule: *the core should be neutral, this is
//! the hard limit*. Core must not NAME, CARRY or RESOLVE a vendor — and this file
//! measures the first of the three, over the ONE part of it Cargo cannot see.
//!
//! # CARGO SUBSUMED THIS CENSUS'S PRODUCTION HALF, AND THAT IS WHY IT NARROWED
//!
//! `zeroship-migrate-core` declares no vendor `[dependencies]`. Writing
//! `zeroship_migrate_postgres` in its production source is an unresolved-crate error, so
//! the rule this file used to hold over production code is now held by the build. That
//! is strictly stronger than a census: it is checked at every compile, it cannot go
//! blind, and it names the offending line itself.
//!
//! WHAT IT DOES NOT COVER is `[dev-dependencies]`. Core's own `#[cfg(test)]` modules
//! hand a `VendorSet` to every resolution door they exercise, and a set has to be
//! composed from real vendors — a fake would make several hundred unit tests assert
//! against a double instead of against the backends that ship. So the three vendor
//! crates ARE dev-dependencies of the engine, and `#[cfg(test)]` code anywhere under
//! `src` can reach them and compile clean.
//!
//! That is the hole, and this file is what keeps it one file wide.
//!
//! # What "name a vendor" means here, precisely
//!
//! Three crate idents — `zeroship_migrate_postgres`, `zeroship_migrate_sqlite`,
//! `zeroship_migrate_mysql` — appearing anywhere in the engine's source. Every place a
//! `#[cfg(test)]` module writes one is a place where a test resolved a vendor WITHOUT
//! going through the registry it was handed, and that is exactly the drift the
//! production rule exists to prevent, wearing test clothes: twenty-two files knowing
//! which vendors exist is twenty-two files that a fourth backend has to be added to.
//!
//! So the target is not zero. It is ONE FILE: `test_fixtures.rs`, which composes the
//! `#[cfg(test)]` vendor set once and hands it to everything else. The ratchet is the
//! three entries of that composition.
//!
//! Its previous single entry was `render/backends/mod.rs`, the PRODUCTION registry
//! composition, at the same count of three. That file names no vendor crate now — the
//! composition is `crates/zeroship-migrate/src/lib.rs` — so the ratchet did not move
//! sideways, it moved from a rule Cargo could not enforce to the residue of one it can.
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
//! | `render/vendor.rs` | 1 | `pub use zeroship_migrate_postgres::render_vendor_op` | closed |
//! | `render/declarative.rs` | 1 | `use zeroship_migrate_mysql::collation::{…}` | closed |
//!
//! `render/vendor.rs` closed by putting the vendor-op surface behind
//! `DmlRenderer::render_vendor_op`. That one is now ALSO a privacy rule — `mod vendor`
//! is private in `zeroship-migrate-postgres` and the crate-root `pub use` is gone, so
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
//! `crates/zeroship-migrate-core/src` is dense with prose about the vendor crates — the
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
//! the walk and the `test_fixtures.rs` entry in [`ALLOWED`] for the needle — that
//! composition's three lines are a positive control that must keep matching.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The three vendor crate idents, as they appear in Rust code.
///
/// The underscore spelling is the only one that can be a PATH. A hyphenated
/// `zeroship-migrate-postgres` can only ever be prose or a `Cargo.toml` key, so matching
/// the underscore form is what separates "core reached this crate" from "core talked
/// about it".
const VENDOR_CRATES: &[&str] = &[
    "zeroship_migrate_mysql",
    "zeroship_migrate_postgres",
    "zeroship_migrate_sqlite",
];

/// The ratchet: which core files may name a vendor crate, and exactly how many times.
///
/// Paths are relative to `crates/zeroship-migrate-core/src`. See the module header for what
/// each entry is and which of them is permanent.
const ALLOWED: &[(&str, usize)] = &[
    // The `#[cfg(test)]` fixture module's three-vendor composition. It is the ONE
    // reachable-by-`cfg(test)` naming of a vendor crate left in the engine, and the
    // reason this census outlived the crate split — see the module header.
    ("test_fixtures.rs", 3),
];

/// The walk's floor — the WEAKER of this file's two anti-blindness checks. See
/// [`WALK_ANCHORS`] for the one that actually holds.
///
/// Its premise is inverted and saying so is the point. It was written assuming core
/// churns around a stable size, so a big drop could only mean a broken walk. That is
/// backwards: core is deliberately SHRINKING. Vendor code is being moved out of it
/// on purpose, and the file count has already fallen from the 87 this comment was
/// first written against. A count that falls is the project WORKING.
///
/// So this floor will eventually fire on a correct tree, and its old instruction
/// ("NEVER lower it to get green") would then forbid the only correct response. The
/// rule is therefore narrower than it looks: lower it when an extraction legitimately
/// moved files OUT, and say which extraction in the commit. Never lower it to silence
/// a walk that broke — [`WALK_ANCHORS`] is what tells those two apart, so check it
/// first and trust it over this number.
///
/// It has now fired exactly that way three times, on consecutive extractions.
///
/// - The MySQL execution half — `apply/backend/mysql/`, eight files — moved into
///   `zeroship-migrate-mysql`, taking core from 77 `.rs` files to 69 and under the 70
///   this said. Lowered to 60.
/// - The SQLite execution half — `apply/backend/sqlite/`, ELEVEN files — moved into
///   `zeroship-migrate-sqlite/src/backend/`, taking core from 69 to 58 and under that 60.
///   Lowered to 50.
///
/// - The PostgreSQL execution half — `apply/backend/postgres/`, TEN files — moved
///   into `zeroship-migrate-postgres/src/backend/`, taking core from 58 to 48 and under
///   that 50. Lowered to 42.
///
/// All three times [`WALK_ANCHORS`] passed, which is what said the walk was intact
/// and the shrink was the extraction. 42 is under 48 with room to churn and still far
/// from a walk that found nothing. There is no vendor subtree left in core — this was
/// the last one — so the next time this fires it is core shrinking for some other
/// reason, and that is a question rather than a routine lowering.
const SRC_FILE_FLOOR: usize = 42;

/// Files the walk MUST reach, which is the real defence against a census that fails
/// open.
///
/// A floor over a discovered set only bounds HOW MANY things were found; these bound
/// WHICH, and they stay true at any crate size. `lib.rs` sits at the walk's root, so
/// losing it means the root itself was wrong. `render/backends/mod.rs` is three
/// levels down and is the registry the `ALLOWED` table above calls PERMANENT, so
/// losing it means recursion stopped descending. Between them a walk cannot be
/// narrowed to nothing and still pass, however small core gets.
///
/// Pick replacements only from files that cannot move. Anchoring on something inside
/// `apply/backend/` would have rotted the moment that directory was extracted, and it
/// has been: `apply/backend/` holds `mod.rs` and `capability.rs` now and no vendor.
const WALK_ANCHORS: &[&str] = &["lib.rs", "render/backends/mod.rs"];

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

/// The ENGINE's source root, which is `zeroship-migrate-core/src` and no longer this
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
        .join("zeroship-migrate-core")
        .join("src")
}

/// The census.
#[test]
fn core_names_no_vendor_crate_outside_the_registry() {
    let src = engine_src();
    let files = src_files(&src);

    // FLOOR ONE — the WALK. A scan over a discovered set fails open.
    //
    // The anchors run FIRST because they are the check that survives core shrinking.
    // They answer "did the walk reach the tree at all", which is the actual failure
    // mode; the count below only answers "did it reach a lot of it".
    let reached: std::collections::BTreeSet<String> = files
        .iter()
        .filter_map(|p| p.strip_prefix(&src).ok())
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    for anchor in WALK_ANCHORS {
        assert!(
            reached.contains(*anchor),
            "the census walked {} files under {} but never reached `{anchor}`, so the \
             walk is not seeing the tree it claims to census. This is the failure the \
             floor below is too blunt to catch: fix the walk. Do NOT delete the anchor \
             to get green — if `{anchor}` legitimately moved, point the anchor at \
             another file that cannot move, and say which in the commit.",
            files.len(),
            src.display()
        );
    }

    assert!(
        files.len() >= SRC_FILE_FLOOR,
        "the census walked only {} files under {}, below the floor of {SRC_FILE_FLOOR}. \
         The anchors above PASSED, so the walk did reach the tree and this is most \
         likely core legitimately shrinking as vendor code moves out — lower the floor \
         and name the extraction that did it. If the anchors FAILED, fix the walk \
         instead and ignore this number.",
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

    // FLOOR TWO — the NEEDLE. The `#[cfg(test)]` fixture composition is a positive
    // control: it names all three vendor crates once each, and it is not going to stop
    // — several hundred unit tests need a real vendor set to hand the resolution
    // doors. If this stops matching, the census has gone blind and every other zero
    // below it is meaningless.
    //
    // It USED TO BE `render/backends/mod.rs`, the production registry composition.
    // That file now returns zero for all three idents, and that zero is this census
    // succeeding rather than the matcher failing: the composition left the crate, and
    // the engine no longer declares a vendor dependency to name.
    let fixtures = found.get("test_fixtures.rs").copied().unwrap_or(0);
    assert_eq!(
        fixtures, 3,
        "the needle found {fixtures} vendor-crate names in test_fixtures.rs, \
         expected 3 (the three entries of its `#[cfg(test)]` composition). Either the \
         fixture module was restructured — update this control deliberately — or \
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

/// The engine's manifest declares no vendor `[dependencies]`, which is the PREMISE
/// everything above rests on.
///
/// # Why this test exists, and why the census alone is not enough
///
/// The census above is a source scan. It measures what the engine's code NAMES. The
/// thing that makes naming a vendor impossible in production code is one level down
/// and is not source at all: `crates/zeroship-migrate-core/Cargo.toml` lists
/// `zeroship-migrate-backend`, `zeroship-migrate-ir` and `zeroship-migrate-policy` under
/// `[dependencies]` and no backend implementation, so `zeroship_migrate_postgres` does not
/// resolve there.
///
/// ONE LINE IN THAT MANIFEST UNDOES IT. Add `zeroship-migrate-postgres = { workspace =
/// true }` to `[dependencies]` and the whole structural argument evaporates silently:
/// nothing fails, the tree stays green, and the rule quietly reverts to being whatever
/// this file's ratchet happens to allow. That is the shape of failure this repository
/// keeps finding — a rule whose enforcement mechanism has no artifact of its own.
///
/// So the manifest edge gets its own check. It lives HERE rather than in a new file
/// because it is the same rule as the census above, measured one level down: the census
/// says the engine does not name a vendor, and this says the engine COULD not.
///
/// The vendor crates ARE allowed under `[dev-dependencies]`, and are: the engine's own
/// `#[cfg(test)]` modules compose a real `VendorSet` from them, which is the bounded
/// hole the ratchet above exists to keep bounded.
#[test]
fn the_engine_manifest_declares_no_vendor_dependency() {
    /// The engine's manifest, relative to `crates/`.
    const ENGINE_MANIFEST: &str = "zeroship-migrate-core/Cargo.toml";

    /// The section a vendor edge may appear under, and only that one.
    const ALLOWED_SECTION: &str = "[dev-dependencies]";

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("this crate lives at <workspace>/crates/<name>")
        .join(ENGINE_MANIFEST);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));

    // The hyphenated spelling: a manifest key, never a Rust path. `VENDOR_CRATES`
    // holds the underscore form the source census needs, so the two needles are
    // deliberately different and neither can vouch for the other.
    let vendor_keys: Vec<String> = VENDOR_CRATES.iter().map(|c| c.replace('_', "-")).collect();

    let mut section = String::new();
    let mut edges: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = trimmed.to_string();
            continue;
        }
        if vendor_keys.iter().any(|k| trimmed.starts_with(k.as_str())) {
            edges.push((section.clone(), trimmed.to_string()));
        }
    }

    // ---- THE PROPERTY RUNS FIRST, and the order is deliberate. -----------------
    //
    // The floor below is a needle-liveness control, and a control that fires FIRST on
    // a real violation reports the wrong defect: adding a shipping edge also changes
    // the count, so a count check placed first says "the needle went blind" about a
    // manifest that is simply wrong. Measured, not reasoned — the first version of
    // this test did exactly that when poisoned.
    let shipping: Vec<String> = edges
        .iter()
        .filter(|(section, _)| section != ALLOWED_SECTION)
        .map(|(section, line)| format!("  {section}: {line}"))
        .collect();
    assert!(
        shipping.is_empty(),
        "{ENGINE_MANIFEST} declares a vendor crate outside `{ALLOWED_SECTION}`:\n{}\n\n\
         That one line dissolves the whole structural rule this crate split exists for. \
         The engine is NEUTRAL BY CONSTRUCTION: it cannot name a backend because it \
         cannot see one, and a normal dependency edge makes `zeroship_migrate_postgres` \
         resolve in every module under `src`. Whatever this edge was added for, the \
         answer is a `VendorSet` parameter the composition supplies — that is what \
         every resolution door in `render::backends` already takes.",
        shipping.join("\n")
    );

    // ---- THE FLOOR. Zero dev edges would mean the needle went blind, not that the
    // ---- engine got stricter: the `#[cfg(test)]` composition needs all three, and if
    // ---- it ever stops needing them the ratchet above goes red first.
    let dev = edges.iter().filter(|(s, _)| s == ALLOWED_SECTION).count();
    assert_eq!(
        dev,
        VENDOR_CRATES.len(),
        "found {dev} vendor edge(s) under `{ALLOWED_SECTION}` in {ENGINE_MANIFEST}, \
         expected {} (one per shipping backend, for the `#[cfg(test)]` composition). \
         Either the fixture set stopped needing them — in which case the ratchet above \
         is red too — or this needle stopped matching a manifest key, in which case the \
         property above means nothing.\nfound: {edges:?}",
        VENDOR_CRATES.len()
    );
}
