//! The REPLACEMENT for ten `pub(crate)` invariants the MySQL extraction dissolved,
//! and the sibling of [`constraint_definition_is_comparison_text`] in every respect
//! that matters.
//!
//! # What was lost, exactly
//!
//! The catalog value-format comparison — `catalog_id_default`,
//! `catalog_uuid_id_default`, `catalog_text_id_default`, `recover_format_check`,
//! `RecoveredFormatCheck`, `sql_literal_fingerprint`,
//! `catalog_expression_fingerprint`, `id_default_from_literal_fingerprint`,
//! `uuid_column_metadata` and `column_metadata` — was `pub(crate)` in
//! `zeroship_migrate::render::value_format`. It moved into
//! `zeroship_migrate_backend::value_format` because MySQL's drift path reads every column
//! default and format `CHECK` through it, and `pub(crate)` DOES NOT SURVIVE A CRATE
//! BOUNDARY: the moved file's `crate` is the contract crate now, so `pub(crate)`
//! there would have hidden the items from every caller including the engine.
//!
//! They are `pub` now. The compiler no longer refuses what it used to refuse, and
//! that is a real downgrade, recorded here rather than asserted in prose.
//!
//! # The rule, and why it is not "no vendor may call it"
//!
//! Its sibling [`backend_snapshot_privates_stay_core_only`] asserts a flat ZERO
//! across the vendor crates, because a vendor has no business COMPARING two schemas
//! at all — and it still does, over `index_elements_canonically_eq` and the rest of
//! the diff helpers, which never left the engine. This file cannot say that: a vendor
//! calling `catalog_id_default` to normalize a default it just read out of its own
//! `information_schema` is the entire reason the item moved. The rule is narrower and
//! DIRECTIONAL, and it is the same one the constraint-definition codec carries:
//!
//! **The catalog comparison is what a vendor does with what it READ. It is never
//! part of what a vendor WRITES.**
//!
//! `catalog_id_default` answers "what comparison key does this catalog text mean".
//! Its output is an [`IdDefaultSnapshot`] — a drift key, deliberately lossy, and not
//! SQL. `recover_format_check` is the same shape in reverse: it asks whether a
//! catalog `CHECK` clause IS a format contract, and returns the contract, never the
//! bytes. A vendor that reached either while building a statement would be emitting
//! comparison vocabulary at a server.
//!
//! `column_metadata` and `uuid_column_metadata` are the pair that could most
//! plausibly be misread as emitters, because their answer carries a `ddl_type` and an
//! `inline_check` that really are SQL. They are still not an emission route: the
//! engine calls them to compute the DESIRED shape it will then compare an
//! introspection against, and a vendor's own `ValueFormatRenderer` is what they
//! consult to do it. A vendor's `value_format.rs` calling back into them is a cycle,
//! not a shortcut.
//!
//! # Why no behaviour test covers this
//!
//! For the same reason its sibling gives. A vendor reaching the comparison for
//! emission would, on the cases these tests exercise, produce bytes that happen to be
//! right — `column_metadata`'s `ddl_type` IS this vendor's DDL type — so every
//! emitted-SQL assertion stays green while the layering is inverted. What it costs is
//! the next extraction: a vendor whose emitters depend on the engine's comparison
//! surface cannot be reasoned about as a leaf.
//!
//! # Scope, said out loud
//!
//! EMISSION modules only. `ddl.rs` and `dml.rs` are the two that render statements a
//! server will parse; `value_format.rs` is here as well, because it is the FACTS side
//! of this exact seam and a call from it is the cycle described above.
//!
//! A vendor's `schema.rs` is deliberately NOT here, the same call its sibling makes:
//! `SchemaRenderer::canonical_type` is itself part of the comparison form (MySQL's
//! drift path calls it on every introspected column), so that file is legitimately
//! mixed and a census over it would have to special-case itself into uselessness.
//!
//! A vendor's `backend/` subtree is NOT here either, and that is the point rather
//! than a gap: `backend/drift_sql.rs` is where the legal calls live.
//!
//! This file also says nothing about WHETHER an item should have moved.
//! [`registry_resolution_stays_core_only`] answers that, and it reads the vendor
//! crates too: these functions take their renderers as PARAMETERS rather than
//! resolving them from a `DialectId`, which is what makes them legal in a vendor at
//! all.
//!
//! # The floors, because a scan over a DISCOVERED set fails OPEN
//!
//! Two different blindnesses, so two different floors, and neither is a bare count:
//!
//! 1. [`EMISSION_MODULES`] as WALK ANCHORS. Every one of the nine must be found on
//!    disk. A count alone would let a walk that stopped descending into one crate
//!    still clear it; naming the files bounds WHICH were read.
//! 2. [`COMPARISON_CONTROL_FLOOR`] — a positive control PER ITEM, running the
//!    IDENTICAL matcher where each name must appear. Stated per item rather than as a
//!    total, because a total lets one popular name carry the dead ones.
//!
//! The control's home is ONE file for all ten: `zero-migrate-core/src/render/value_format.rs`,
//! the engine's dialect-resolving doors plus the comparison's own test module, which
//! stayed there because it needs all three vendors and the contract crate has none.
//! Every one of the ten is named there today, so no needle is uncontrolled — which is
//! the failure mode the sibling censuses warn about when a per-item floor is replaced
//! by a total.
//!
//! [`IdDefaultSnapshot`]: zeroship_migrate::model::snapshot::IdDefaultSnapshot

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The ten degraded items, by the ident a caller would have to write.
///
/// `CatalogRules` and `VendorRules` are NOT here and were not degraded: they are the
/// PARAMETER the comparison takes, they were introduced `pub` by the same commit that
/// moved the bodies, and a vendor naming `VendorRules` is that vendor stating whose
/// rules its own snapshots normalize under — the design, not a breach.
const COMPARISON_ITEMS: &[&str] = &[
    "catalog_id_default",
    "catalog_uuid_id_default",
    "catalog_text_id_default",
    "recover_format_check",
    "RecoveredFormatCheck",
    "sql_literal_fingerprint",
    "catalog_expression_fingerprint",
    "id_default_from_literal_fingerprint",
    "uuid_column_metadata",
    "column_metadata",
];

/// The vendor crates, relative to the workspace `crates/` directory.
const VENDOR_CRATES: &[&str] = &[
    "zero-migrate-postgres",
    "zero-migrate-sqlite",
    "zero-migrate-mysql",
];

/// The EMISSION modules inside each vendor crate — the files whose output a server
/// parses, plus the renderer this comparison consults.
///
/// All three exist in all three vendor crates, which is what makes them usable as
/// walk anchors: a missing one means the layout changed and this census needs
/// rereading, not that the tree got cleaner.
const EMISSION_MODULES: &[&str] = &["ddl.rs", "dml.rs", "value_format.rs"];

/// How many CODE lines in [`ENGINE_DOORS`] must name each item, PER ITEM.
///
/// The identical matcher, run where the answer must not be zero. A broken needle
/// returns a confident ZERO, and every vendor zero this file reports would then mean
/// nothing. Stated per item rather than as a total, because a total lets one popular
/// name carry the dead ones: `catalog_expression_fingerprint` alone would clear any
/// aggregate this file could sensibly set.
///
/// Measured when written: `catalog_id_default` 27, `catalog_uuid_id_default` 6,
/// `catalog_text_id_default` 3, `recover_format_check` 10, `RecoveredFormatCheck` 8,
/// `sql_literal_fingerprint` 4, `catalog_expression_fingerprint` 36,
/// `id_default_from_literal_fingerprint` 3, `uuid_column_metadata` 1,
/// `column_metadata` 5. The floors sit below those with room to churn; they only have
/// to prove the matcher is not blind, so they are blunt on purpose.
///
/// An item whose count here reaches zero has genuinely left that file — repoint or
/// retire its entry deliberately in the commit that did it, rather than dropping the
/// floor to get green.
const COMPARISON_CONTROL_FLOOR: &[(&str, usize)] = &[
    ("catalog_id_default", 10),
    ("catalog_uuid_id_default", 3),
    ("catalog_text_id_default", 2),
    ("recover_format_check", 4),
    ("RecoveredFormatCheck", 4),
    ("sql_literal_fingerprint", 2),
    ("catalog_expression_fingerprint", 12),
    ("id_default_from_literal_fingerprint", 2),
    ("uuid_column_metadata", 1),
    ("column_metadata", 3),
];

/// The engine's dialect-resolving doors, relative to `crates/`. It carries the
/// comparison's own test module too, which is why every one of the ten is reachable
/// from this single control.
const ENGINE_DOORS: &str = "zero-migrate-core/src/render/value_format.rs";

/// Whether a source line is CODE rather than a comment.
///
/// The same line-oriented filter every sibling census uses, with the same stated
/// limits: a `//`, `///`, `//!` line is prose, a line whose first non-space character
/// is `*` is a block-comment continuation, and everything else is code. It earns its
/// keep rather than hypothetically — `zero-migrate-mysql/src/value_format.rs` carries
/// doc comments naming `recover_format_check`, which are prose about the rule and not
/// a breach of it.
fn is_code(line: &str) -> bool {
    let t = line.trim_start();
    !(t.starts_with("//") || t.starts_with('*') || t.starts_with("/*"))
}

/// Whether `c` can appear inside a Rust identifier.
fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// How many times `line` REACHES `name` — as a free call, a path segment, or a type.
///
/// Deliberately wider than a call matcher, because `RecoveredFormatCheck` is an enum
/// and `use` lines carry no parentheses. A hit needs `name` bounded by non-identifier
/// characters, and is then discounted in exactly two cases:
///
/// * preceded by `.` — `self.column_metadata(…)` is a vendor answering its own trait
///   method, not calling the comparison; and
/// * preceded by the keyword `fn` — implementing a same-named trait method is not a
///   call to the free function.
///
/// Both discounts are load-bearing rather than cautious: `uuid_column_metadata` and
/// `column_metadata` are ALSO `ValueFormatRenderer` method names, which every vendor's
/// `value_format.rs` implements, so without them this census would be red on day one
/// for the one thing each vendor is required to do.
fn reach_sites(line: &str, name: &str) -> usize {
    let mut count = 0;
    let mut from = 0;
    while let Some(offset) = line[from..].find(name) {
        let start = from + offset;
        let end = start + name.len();
        from = end;
        if line[end..].chars().next().is_some_and(is_ident_char) {
            continue;
        }
        let before = &line[..start];
        if before
            .chars()
            .last()
            .is_some_and(|c| c == '.' || is_ident_char(c))
        {
            continue;
        }
        let head = before.trim_end();
        let defines = head.ends_with("fn")
            && head[..head.len() - "fn".len()]
                .chars()
                .last()
                .is_none_or(|c| !is_ident_char(c));
        if !defines {
            count += 1;
        }
    }
    count
}

/// Code lines of `path` that reach one of [`COMPARISON_ITEMS`], keyed by item.
fn reaches(path: &Path, label: &str) -> BTreeMap<String, Vec<String>> {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let mut found: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (number, line) in text.lines().enumerate().filter(|(_, l)| is_code(l)) {
        for item in COMPARISON_ITEMS {
            if reach_sites(line, item) > 0 {
                found.entry((*item).to_string()).or_default().push(format!(
                    "{label}:{}: {}",
                    number + 1,
                    line.trim()
                ));
            }
        }
    }
    found
}

fn crates_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/zero-migrate has a parent")
        .to_path_buf()
}

#[test]
fn the_catalog_comparison_is_never_reached_from_a_vendors_emitters() {
    let crates = crates_root();

    // ---- FLOOR ONE: the WALK, bounded by WHICH files, not how many. ----------
    let mut emitters: Vec<(String, PathBuf)> = Vec::new();
    for vendor in VENDOR_CRATES {
        for module in EMISSION_MODULES {
            let path = crates.join(vendor).join("src").join(module);
            assert!(
                path.is_file(),
                "emission module {} does not exist, so this census would walk one \
                 fewer file and report clean. Fix the list. Do NOT delete the entry \
                 to get green — if the module legitimately moved, point the anchor at \
                 wherever that vendor emits now and say so in the commit.",
                path.display()
            );
            emitters.push((format!("{vendor}/src/{module}"), path));
        }
    }
    assert_eq!(
        emitters.len(),
        VENDOR_CRATES.len() * EMISSION_MODULES.len(),
        "the emitter listing lost an entry"
    );

    // ---- FLOOR TWO: the NEEDLE, as positive controls. ------------------------
    //
    // The identical matcher, run where the answer must not be zero. A broken needle
    // returns a confident ZERO, and every vendor zero below would then mean nothing.
    let control = crates.join(ENGINE_DOORS);
    assert!(
        control.is_file(),
        "the positive control's home {} does not exist, so the control itself is \
         blind and cannot vouch for the needles.",
        control.display()
    );
    let control_hits = reaches(&control, ENGINE_DOORS);
    let mut blind: Vec<String> = Vec::new();
    for (item, floor) in COMPARISON_CONTROL_FLOOR {
        let n = control_hits.get(*item).map_or(0, Vec::len);
        if n < *floor {
            blind.push(format!(
                "  {item}: the control found {n} code line(s) in {ENGINE_DOORS}, floor \
                 {floor}"
            ));
        }
    }
    assert_eq!(
        COMPARISON_CONTROL_FLOOR.len(),
        COMPARISON_ITEMS.len(),
        "every item this census forbids in an emitter must also carry a positive \
         control; an uncontrolled needle is one that can go blind unnoticed"
    );
    assert!(
        blind.is_empty(),
        "the census has gone BLIND — these needles no longer match where they must:\n\
         {}\n\nEither the item genuinely left that file (retire or repoint its entry \
         deliberately, in the commit that did it) or the matcher stopped working, in \
         which case every vendor zero this file reports is meaningless. Do not lower a \
         floor to get green.",
        blind.join("\n")
    );

    // ---- THE PROPERTY. ------------------------------------------------------
    let mut violations: Vec<String> = Vec::new();
    for (label, path) in &emitters {
        for (item, sites) in reaches(path, label) {
            for site in sites {
                violations.push(format!("  [{item}] {site}"));
            }
        }
    }
    violations.sort();
    assert!(
        violations.is_empty(),
        "a vendor's EMITTER reaches the catalog value-format comparison:\n{}\n\nThe \
         comparison is what a backend does with what it READ — its drift path may \
         normalize an introspected default or recover a format CHECK through it, and \
         that is why it is `pub` at all. It is never part of what a backend WRITES. \
         `catalog_id_default` answers in drift keys, not SQL; `column_metadata` \
         answers the DESIRED shape the engine will compare an introspection against, \
         and it consults this vendor's own `ValueFormatRenderer` to do it, so a call \
         from `value_format.rs` is a cycle rather than a shortcut.\n\nThese items were \
         `pub(crate)` in `zeroship_migrate::render::value_format` until the MySQL \
         execution half moved into a vendor crate; `pub(crate)` cannot cross a crate \
         boundary, so this census is what is left of that refusal.",
        violations.join("\n")
    );
}
