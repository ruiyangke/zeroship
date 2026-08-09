//! Resolves every recorded op fixture through the REAL table-shape resolver and
//! compares the result to the committed `<stem>.golden.json`.
//!
//! This is one half of a two-part check. The other half
//! (`packages/zero-migrate/tests/recorded-corpus.test.ts`) executes each
//! `op_fixtures/<stem>.mig.js` through the production recorder and asserts the
//! drained envelope equals `op_fixtures/recorded.json`. This half reads that same
//! file, rebuilds an envelope from the recorded `{ name, ops }`, runs
//! [`resolve_create_table_policy`] under the shared confined charter, and asserts
//! the resolved ops equal the golden. Composed, the halves check `.mig.js` ->
//! golden for all 26 stems, with each half in the job that already has its
//! toolchain: no new public API and no new CI step.
//!
//! The resolver used here is the SAME function the addon's production lowering
//! calls (`zero-migrate-node/src/lower.rs`), not a reimplementation. A JS
//! re-derivation of the policy would only prove the test agrees with itself; the
//! goldens are byte-identical to what the shipped path produces because the shipped
//! path produced them.
//!
//! Nothing here writes a golden. There is deliberately no re-bless environment
//! variable in either half: an easy update affordance is precisely what converts a
//! corpus into a mirror of whatever the code emits today, so regenerating a golden
//! is a hand edit that shows up in review as one.

mod support;

use std::collections::BTreeSet;
use std::path::PathBuf;

use zero_migrate::model::ir::Op;
use zero_migrate::{resolve_create_table_policy, MigrationIr, CURRENT_IR_VERSION};

const MIG_SUFFIX: &str = ".mig.js";
const GOLDEN_SUFFIX: &str = ".golden.json";
const RECORDED_FILE: &str = "recorded.json";

/// The schema unqualified objects resolve under. The confined charter's `[[inject]]`
/// is `scope = "all"`, so this names objects rather than selecting a policy.
const DEFAULT_SCHEMA: &str = "public";

/// The corpus, committed rather than globbed. A directory listing cannot notice a
/// fixture that went missing, so this list is the authority and the directory is
/// checked against it in both directions below.
const EXPECTED_STEMS: [&str; 26] = [
    "alter_primary_key",
    "comments_indexes",
    "constraint_not_valid",
    "ddl_addcol_constraints",
    "ddl_alter",
    "ddl_create",
    "ddl_drop",
    "ddl_rename_table",
    "dialectal_ops",
    "dml",
    "dml_upsert",
    "edge_scalars",
    "enums_domains",
    "fluent_ddl",
    "fluent_dml",
    "fluent_scalars",
    "grouped_views",
    "in_list_scalars",
    "p2a_facets",
    "partition",
    "pg_aggregates",
    "pg_vendor",
    "runtime_options",
    "sequences_exclusion",
    "synchronize_identity",
    "views",
];

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/op_fixtures")
}

fn read_recorded() -> serde_json::Map<String, serde_json::Value> {
    let path = fixtures_dir().join(RECORDED_FILE);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str::<serde_json::Value>(&text)
        .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
        .as_object()
        .unwrap_or_else(|| panic!("{} is a JSON object keyed by stem", path.display()))
        .clone()
}

fn read_golden(stem: &str) -> MigrationIr {
    let path = fixtures_dir().join(format!("{stem}{GOLDEN_SUFFIX}"));
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// Rebuild a full envelope from one recorded `{ name, ops }` entry -- the raw author
/// ops, before any policy resolution. `ir_version` is supplied here rather than
/// carried across the join because a `.mig.js` does not determine it: the host
/// stamps the authoritative [`CURRENT_IR_VERSION`]. Every remaining `MigrationIr`
/// field is serde-defaulted; none is determined by a fixture and none is compared.
/// `deny_unknown_fields` on `MigrationIr` rejects an entry carrying anything else.
fn authored_envelope(stem: &str, raw: &serde_json::Value) -> MigrationIr {
    let mut object = raw
        .as_object()
        .unwrap_or_else(|| panic!("{RECORDED_FILE} entry {stem} is a JSON object"))
        .clone();
    object.insert("ir_version".to_string(), CURRENT_IR_VERSION.into());
    serde_json::from_value(serde_json::Value::Object(object))
        .unwrap_or_else(|e| panic!("{RECORDED_FILE} entry {stem} is a valid envelope: {e}"))
}

fn pretty(ops: &[Op]) -> String {
    serde_json::to_string_pretty(ops).expect("ops serialize")
}

#[test]
fn op_fixture_corpus_is_exactly_the_committed_stem_list() {
    let expected: BTreeSet<&str> = EXPECTED_STEMS.iter().copied().collect();
    assert_eq!(
        expected.len(),
        EXPECTED_STEMS.len(),
        "the stem list has no duplicates"
    );

    let mut mig_stems: BTreeSet<String> = BTreeSet::new();
    let mut golden_stems: BTreeSet<String> = BTreeSet::new();
    let mut unrecognized: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(fixtures_dir()).expect("read op_fixtures") {
        let name = entry.expect("dir entry").file_name();
        let name = name.to_str().expect("fixture names are UTF-8").to_string();
        // No skip branch: an entry matching nothing is a failure, not a pass. A loop
        // that quietly continues past unmatched entries lets the corpus shrink
        // without any test noticing.
        if let Some(stem) = name.strip_suffix(MIG_SUFFIX) {
            mig_stems.insert(stem.to_string());
        } else if let Some(stem) = name.strip_suffix(GOLDEN_SUFFIX) {
            golden_stems.insert(stem.to_string());
        } else if name != RECORDED_FILE {
            unrecognized.push(name);
        }
    }
    assert!(
        unrecognized.is_empty(),
        "every op_fixtures entry is a {MIG_SUFFIX}, a {GOLDEN_SUFFIX}, or {RECORDED_FILE}; \
         found {unrecognized:?}"
    );

    let expected: BTreeSet<String> = EXPECTED_STEMS.iter().map(|s| (*s).to_string()).collect();
    assert_eq!(
        mig_stems, expected,
        "the {MIG_SUFFIX} set equals the committed stem list"
    );
    assert_eq!(
        golden_stems, expected,
        "the {GOLDEN_SUFFIX} set equals the committed stem list"
    );
    let recorded_stems: BTreeSet<String> = read_recorded().keys().cloned().collect();
    assert_eq!(
        recorded_stems, expected,
        "the {RECORDED_FILE} key set equals the committed stem list"
    );
}

#[test]
fn recorded_fixtures_resolve_to_their_committed_goldens() {
    let recorded = read_recorded();
    let charter = support::confined_charter();
    let mut compared = 0usize;
    // Enumerated from the AUTHORING INPUTS by way of the committed stem list. Keying
    // this loop on `*.golden.json` is the defect being closed: it lets an input drift
    // away from the artifact it is supposed to produce with no assertion ever running.
    for stem in EXPECTED_STEMS {
        let raw = recorded
            .get(stem)
            .unwrap_or_else(|| panic!("{RECORDED_FILE} carries an entry for {stem}"));
        let authored = authored_envelope(stem, raw);
        assert!(
            !authored.ops.is_empty(),
            "{stem} has a non-empty recorded op list"
        );

        let resolved = resolve_create_table_policy(&authored, &charter, DEFAULT_SCHEMA)
            .unwrap_or_else(|e| panic!("{stem} resolves under the confined charter: {e}"));

        let golden = read_golden(stem);
        assert!(
            !golden.ops.is_empty(),
            "{stem} has a non-empty golden op list"
        );
        assert_eq!(
            resolved.name, golden.name,
            "{stem} resolves to the golden name"
        );
        // Counts first, so an empty list can never match an empty list by accident
        // and read as a pass.
        assert_eq!(
            resolved.ops.len(),
            golden.ops.len(),
            "{stem} resolves to the golden op count\n--- resolved ---\n{}\n--- golden ---\n{}",
            pretty(&resolved.ops),
            pretty(&golden.ops),
        );
        assert_eq!(
            resolved.ops,
            golden.ops,
            "{stem} resolves to the committed golden\n--- resolved ---\n{}\n--- golden ---\n{}",
            pretty(&resolved.ops),
            pretty(&golden.ops),
        );
        compared += 1;
    }
    // The pin that catches the failure the other pins cannot: a loop that enumerated
    // every stem and asserted on none of them.
    assert_eq!(
        compared,
        EXPECTED_STEMS.len(),
        "every stem in the corpus was compared"
    );
}
