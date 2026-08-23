//! The REPLACEMENT for the `pub(crate)` that used to make the constraint-definition
//! codec unreachable from a vendor, written in the commit that dissolved it.
//!
//! # What was lost, exactly
//!
//! `quote_ident_if_needed`, `constraintdef_cols` and `NOT_VALID_DEFINITION_SUFFIX`
//! were `pub(crate)` in `zero_migrate::render::declarative`, and
//! `fk_definition_for_dialect` was private to that file. They are now `pub` in
//! `zero_migrate_backend::constraint_definition`, because MySQL's drift path BUILDS
//! the constraint body — its `information_schema` stores no rendered constraint text
//! — and `crates/zero-migrate/src/apply/backend/mysql/` has been extracted into
//! `zero-migrate-mysql`, which cannot depend on the engine.
//!
//! `pub(crate)` DOES NOT SURVIVE A CRATE BOUNDARY, and there is no modifier meaning
//! "visible to the engine and to a vendor's DRIFT path but not to its EMISSION
//! path". So the rule has to be written here instead of asserted in prose and
//! forgotten — which is exactly what it was until now: the old home said it in a
//! comment, in capitals, and nothing checked it.
//!
//! # The rule, and why it is not "no vendor may call it"
//!
//! Its sibling census `backend_snapshot_privates_stay_core_only` asserts a flat ZERO
//! across the vendor crates, because a vendor has no business COMPARING two schemas
//! at all. This one cannot, and must not, say that: a vendor calling
//! `constraintdef_cols` to normalize a key it just read out of its own catalog is
//! the entire reason the item moved. The rule is narrower and it is DIRECTIONAL:
//!
//! **The codec is COMPARISON text. It is never an emitted identifier route.**
//!
//! Every constraint body is built once in the `pg_get_constraintdef` normal form —
//! conditional double quotes, `ON UPDATE` before `ON DELETE`, the canonical default
//! action omitted — so that a desired snapshot and a live introspected one compare
//! equal on every dialect without a per-vendor special case. Those are comparison
//! bytes that merely LOOK like DDL.
//!
//! A vendor that reached for `quote_ident_if_needed` to spell an identifier it was
//! about to EMIT would be using a PostgreSQL-shaped codec as a quoting helper. On
//! MySQL that is simply wrong — InnoDB delimits identifiers with backticks, so the
//! emitted statement would carry `"x"` where it needs `` `x` ``. `zero-migrate-mysql`
//! already keeps the two apart the correct way: `ddl.rs` takes the body it is handed
//! and re-spells it through `mysql_requote_sql`, precisely because building and
//! emitting are different jobs.
//!
//! # Why no behaviour test covers this
//!
//! Because on PostgreSQL and SQLite the wrong call is INDISTINGUISHABLE from the
//! right one: both spell an identifier `"x"`, so a vendor reaching the comparison
//! codec for emission emits correct bytes and every test stays green. That is the
//! same class of latent coupling `render::backends`'s header records for
//! `dml::quote_ident`, where all four SQLite identifier emissions went through the
//! PostgreSQL renderer and were correct only by coincidence. It shows up as a defect
//! on the third backend, which is the one that has not moved yet.
//!
//! # Scope, said out loud
//!
//! EMISSION modules only — each vendor's `ddl.rs` and `dml.rs`, the two that render
//! statements a server will parse. A vendor's `schema.rs` is deliberately NOT here:
//! `SchemaRenderer::canonical_fk_target` is itself part of the comparison form, so
//! that file is legitimately mixed and a census over it would have to special-case
//! itself into uselessness.
//!
//! This file also says nothing about WHETHER an item should have moved.
//! `registry_resolution_stays_core_only` answers that, and it reads the vendor
//! crates too: `fk_definition` and `fk_constraint_snapshot` take a `&BackendVendor`
//! PARAMETER rather than resolving one, which is what makes them legal there at all.
//!
//! # The floors, because a scan over a DISCOVERED set fails OPEN
//!
//! Two different blindnesses, so two different floors, and neither is a bare count:
//!
//! 1. [`EMISSION_MODULES`] as WALK ANCHORS. Every one of the six must be found on
//!    disk. A count alone would let a walk that stopped descending into one crate
//!    still clear it; naming the files bounds WHICH were read.
//! 2. [`CODEC_CONTROL_FLOOR`] — a positive control PER ITEM, running the identical
//!    matcher over the codec's own home, where each name must appear. Stated per
//!    item rather than as a total, because a total lets one popular name carry the
//!    dead ones.
//!
//! The comment filter earns its keep here rather than hypothetically:
//! `zero-migrate-mysql/src/ddl.rs` names `constraintdef_cols` in a doc comment
//! today, explaining the re-spelling above. That line is prose about the rule, not a
//! breach of it, and [`is_code`] is what tells them apart.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The items whose visibility widened when the codec moved below the vendors.
///
/// `normalize_fk_action` is deliberately absent: it was already `pub` in
/// `zero_migrate::schema::query` before the move, so nothing about it degraded, and
/// a bare FK-action keyword is not an identifier route.
const CODEC_ITEMS: &[&str] = &[
    "quote_ident_if_needed",
    "constraintdef_cols",
    "NOT_VALID_DEFINITION_SUFFIX",
    "fk_definition",
    "fk_constraint_snapshot",
    "normalize_fk_action_for_vendor",
];

/// The vendor crates, relative to the workspace `crates/` directory.
const VENDOR_CRATES: &[&str] = &[
    "zero-migrate-postgres",
    "zero-migrate-sqlite",
    "zero-migrate-mysql",
];

/// The EMISSION modules inside each vendor crate — the files whose output a server
/// parses, and therefore the files where the comparison codec must never appear.
///
/// Both exist in all three vendor crates, which is what makes them usable as walk
/// anchors: a missing one means the layout changed and this census needs rereading,
/// not that the tree got cleaner.
const EMISSION_MODULES: &[&str] = &["ddl.rs", "dml.rs"];

/// The codec's own home, used as the needle's positive control.
const CODEC_HOME: &str = "zero-migrate-backend/src/constraint_definition.rs";

/// How many CODE lines in [`CODEC_HOME`] must name each item, per item.
///
/// One apiece: every one of them is at least defined there. Set low and blunt on
/// purpose — the number's only job is to prove the matcher is not blind, and a tight
/// number would be churn every time the module is edited. NEVER lower it to get
/// green; a zero means the item was renamed or removed and the vendor zeroes below
/// it stopped meaning anything.
const CODEC_CONTROL_FLOOR: usize = 1;

/// Whether a source line is CODE rather than a comment.
///
/// Line-oriented on purpose, and the same filter its sibling censuses
/// `core_names_no_vendor_crate.rs` and `backend_snapshot_privates_stay_core_only.rs`
/// use, with the same stated limits: it cannot see inside a block comment that opens
/// mid-line and does not try. It over-counts prose into code, never the reverse,
/// which is the safe direction for a census asserting a ZERO.
fn is_code(line: &str) -> bool {
    let t = line.trim_start();
    !(t.starts_with("//") || t.starts_with('*') || t.starts_with("/*"))
}

/// Code lines in `path` naming `item`.
fn hits(path: &Path, item: &str) -> usize {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    text.lines()
        .filter(|l| is_code(l))
        .filter(|l| l.contains(item))
        .count()
}

/// The workspace `crates/` directory.
fn crates_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/zero-migrate has a parent")
        .to_path_buf()
}

#[test]
fn no_vendor_emission_module_spells_the_comparison_codec() {
    let crates = crates_dir();

    // FLOOR ONE — the WALK, as ANCHORS rather than a count. Every emission module
    // must be on disk; a census that silently read five of six files, or none, would
    // otherwise report clean.
    let mut emission_files = Vec::new();
    for vendor in VENDOR_CRATES {
        for module in EMISSION_MODULES {
            let path = crates.join(vendor).join("src").join(module);
            assert!(
                path.is_file(),
                "the emission module {} does not exist. The census would read nothing \
                 there and report clean; fix the anchor, do not drop it.",
                path.display()
            );
            emission_files.push(path);
        }
    }
    assert_eq!(
        emission_files.len(),
        VENDOR_CRATES.len() * EMISSION_MODULES.len(),
        "the anchor list did not resolve to one file per vendor per emission module"
    );

    // FLOOR TWO — the NEEDLE, as a positive control PER ITEM. The identical matcher,
    // over the codec's own home, where every one of these names is at least defined.
    // A zero here means the matcher went blind and the vendor zeroes are meaningless.
    let home = crates.join(CODEC_HOME);
    assert!(
        home.is_file(),
        "the codec home {} does not exist, so the positive control cannot run",
        home.display()
    );
    for item in CODEC_ITEMS {
        let n = hits(&home, item);
        assert!(
            n >= CODEC_CONTROL_FLOOR,
            "the positive control found {n} code lines naming `{item}` in \
             {CODEC_HOME}, below the floor of {CODEC_CONTROL_FLOOR}. Either the item \
             was renamed or removed — in which case update `CODEC_ITEMS` deliberately \
             — or the needle stopped matching and this census is blind."
        );
    }

    // THE PROPERTY.
    let mut violations: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for path in &emission_files {
        for item in CODEC_ITEMS {
            if hits(path, item) > 0 {
                let rel = path
                    .strip_prefix(&crates)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/");
                violations.entry(rel).or_default().push(item);
            }
        }
    }
    assert!(
        violations.is_empty(),
        "a vendor's EMISSION module names the constraint-definition comparison \
         codec:\n{}\n\nThat codec builds the `pg_get_constraintdef` COMPARISON form — \
         conditional double quotes on every dialect. It is not a quoting helper: a \
         MySQL statement needs backticks, so emitting from here is wrong in a way \
         PostgreSQL and SQLite cannot show you, because both spell an identifier the \
         same as the comparison form does. Build the body, then re-spell it for \
         emission the way `zero-migrate-mysql`'s `mysql_requote_sql` does.",
        violations
            .iter()
            .map(|(f, items)| format!("  {f}: {}", items.join(", ")))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
