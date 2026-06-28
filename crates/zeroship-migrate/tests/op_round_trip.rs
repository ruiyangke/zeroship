//! The PR1 single-source-of-truth anti-drift gate (design §2.5 / PR1 "skeletal JS
//! builder + round-trip fixtures").
//!
//! This is the load-bearing cross-implementation gate the design names as the
//! AUTHORITATIVE anti-drift mechanism: a corpus of `.ir.json` files that BOTH the
//! JS `op.*` builder and the Rust engine must agree on (value equality via the
//! typed-value checksum `Checksum::of_ir`). The IR wire shape is frozen, so this
//! gate must exist at freeze time — it is what guarantees the JS builder and the
//! Rust loader never drift in their interpretation of an op list.
//!
//! Three gates, per the normative §2.5 (mechanisms 1–3):
//!
//! 1. **Golden corpus byte-stability.** Each `op_fixtures/<name>.mig.js` is recorded
//!    by the REAL V8 `op.*` recorder ([`record_migration_to_json_unsandboxed`]) and its
//!    `.ir.json` is gated against the committed `op_fixtures/<name>.ir.json`. Run
//!    `UPDATE_CORPUS=1 cargo test -p zeroship-migrate --test op_round_trip` to
//!    regenerate after an intentional shape change, then commit the goldens.
//!
//! 2. **JS↔Rust value-checksum round-trip.** For each fixture, the JS-emitted op
//!    list and the committed-golden op list (re-parsed + re-canonicalized by Rust)
//!    are folded through the SAME `Checksum::of_ir` — and asserted EQUAL. This is
//!    invariant under any JCS-formatting difference between the two serializers
//!    (§2.5): what is gated is typed-VALUE equality, not raw-byte equality.
//!
//! 3. **Variant-exhaustiveness.** Every `Op` discriminant in `op-ir.schema.json`
//!    appears in at least one fixture — so adding an `Op` variant without a fixture
//!    fails CI (the IR shape can NOT be frozen with an un-gated op).
//!
//! These tests carry V8 (via the recorder) and live with the merged
//! `zeroship-migrate` frontend test suite.

use std::collections::BTreeSet;
use std::path::PathBuf;

use zeroship_migrate::model::ir::CanonicalOpList;
use zeroship_migrate::frontend::{
    record_migration_to_json_unsandboxed, spawn_sandboxed_record, RecordRequest, ResourceBudget,
    SandboxPosture,
};
use zeroship_migrate::{Checksum, MigrationFlags, MigrationIr};

const OWNER: &str = "app_corpus";

/// The fixtures dir (`tests/op_fixtures/`).
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/op_fixtures")
}

/// Every `<name>.mig.js` fixture stem, sorted.
fn fixture_stems() -> Vec<String> {
    let mut stems = Vec::new();
    for entry in std::fs::read_dir(fixtures_dir()).expect("read op_fixtures") {
        let path = entry.expect("dir entry").path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if let Some(stem) = name.strip_suffix(".mig.js") {
            stems.push(stem.to_string());
        }
    }
    stems.sort();
    assert!(!stems.is_empty(), "the op.* corpus must have at least one .mig.js fixture");
    stems
}

/// The authoritative `Checksum::of_ir` over a loaded IR's op list (PR1 default-flags
/// domain — the same the engine journals as the drift anchor).
fn anchor(ir: &MigrationIr) -> Checksum {
    Checksum::of_ir(
        &CanonicalOpList(&ir.ops),
        &MigrationFlags::default(),
        &ir.owner_app,
        &[],
        &[],
        &ir.preconditions,
    )
}

fn record_via_child_to_json(mig_src: &str, stem: &str) -> String {
    let req = RecordRequest {
        ts_source: mig_src.to_string(),
        owner_app: OWNER.to_string(),
        name: stem.to_string(),
        posture: SandboxPosture::Local,
        budget: ResourceBudget::default(),
        allow_read_paths: vec![],
        schema_types_blob: None,
    };
    let res = spawn_sandboxed_record(&req)
        .unwrap_or_else(|e| panic!("sandboxed child record {stem}: {e}"));
    let env: serde_json::Value = serde_json::from_str(&res.ir_json)
        .unwrap_or_else(|e| panic!("{stem}: child envelope is invalid JSON: {e}"));
    assert_eq!(
        env.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "{stem}: child envelope must be ok:true: {}",
        res.ir_json
    );
    let ir_value = env
        .get("ir")
        .cloned()
        .unwrap_or_else(|| panic!("{stem}: child envelope missing ir: {}", res.ir_json));
    let ir: MigrationIr = serde_json::from_value(ir_value)
        .unwrap_or_else(|e| panic!("{stem}: child IR violates MigrationIr: {e}"));
    let mut json = serde_json::to_string_pretty(&ir)
        .unwrap_or_else(|e| panic!("{stem}: serialize child IR: {e}"));
    json.push('\n');
    json
}

/// GATE 1 + 2: for every fixture, the JS-recorded `.ir.json` is byte-stable against
/// the committed golden AND its op-list checksum value-equals the golden's.
#[test]
fn corpus_is_byte_stable_and_value_equal() {
    let update = std::env::var("UPDATE_CORPUS").is_ok();
    for stem in fixture_stems() {
        let mig_src = std::fs::read_to_string(fixtures_dir().join(format!("{stem}.mig.js")))
            .unwrap_or_else(|e| panic!("read {stem}.mig.js: {e}"));

        // Record through the REAL V8 op.* recorder → canonical `.ir.json` string.
        let recorded = record_migration_to_json_unsandboxed(&mig_src, OWNER, &stem)
            .unwrap_or_else(|e| panic!("record {stem}: {e}"));

        let golden_path = fixtures_dir().join(format!("{stem}.ir.json"));
        if update {
            std::fs::write(&golden_path, recorded.as_bytes())
                .unwrap_or_else(|e| panic!("write {stem}.ir.json: {e}"));
            continue;
        }

        // GATE 1 — byte-stability against the committed golden.
        let golden = std::fs::read_to_string(&golden_path).unwrap_or_else(|e| {
            panic!(
                "{stem}.ir.json missing or unreadable ({e}). Run \
                 `UPDATE_CORPUS=1 cargo test -p zeroship-migrate --test op_round_trip` \
                 to generate the corpus, then commit it."
            )
        });
        assert_eq!(
            recorded, golden,
            "{stem}.ir.json is stale (the JS recorder's output drifted from the committed \
             golden). Regenerate with UPDATE_CORPUS=1 and commit."
        );

        // Both the JS-emitted bytes and the committed golden must deserialize through
        // the REAL frozen `MigrationIr` contract (camelCase ops, closed AST, the
        // numeric domain) — the gate biting if the JS emitted an out-of-contract op.
        let from_js: MigrationIr = serde_json::from_str(&recorded)
            .unwrap_or_else(|e| panic!("{stem}: JS-recorded .ir.json violates the IR contract: {e}"));
        let from_golden: MigrationIr = serde_json::from_str(&golden)
            .unwrap_or_else(|e| panic!("{stem}: committed golden violates the IR contract: {e}"));

        // GATE 2 — the op-list checksum value-equals across the two sides. This is
        // the design's AUTHORITATIVE anti-drift check: equal typed VALUES ⇒ equal
        // `Checksum::of_ir`, invariant under JCS-formatting differences.
        assert_eq!(
            anchor(&from_js).as_str(),
            anchor(&from_golden).as_str(),
            "{stem}: Checksum::of_ir(JS-emitted) != Checksum::of_ir(Rust-recanonicalized golden) \
             — the JS builder and the Rust engine disagree on the op list (single-source drift)"
        );
    }
}

/// The sandboxed child and the in-process oracle must share the same module graph.
/// This pins the vendor subpath too: `pg_vendor.mig.js` imports
/// `@zeroship/migrate/pg`, which pre-change resolved in-process but not in the child.
#[test]
fn sandboxed_child_matches_in_process_corpus_including_pg_vendor_subpath() {
    for stem in fixture_stems() {
        let mig_src = std::fs::read_to_string(fixtures_dir().join(format!("{stem}.mig.js")))
            .unwrap_or_else(|e| panic!("read {stem}.mig.js: {e}"));

        let in_process = record_migration_to_json_unsandboxed(&mig_src, OWNER, &stem)
            .unwrap_or_else(|e| panic!("in-process record {stem}: {e}"));
        let child = record_via_child_to_json(&mig_src, &stem);

        assert_eq!(
            child, in_process,
            "{stem}: sandboxed child recorder drifted from in-process oracle"
        );
    }
}

/// GATE 3 — variant-exhaustiveness (§2.5 mechanism 3): every `Op` discriminant the
/// frozen `op-ir.schema.json` enumerates appears in at least one corpus fixture.
/// Adding an `Op` variant without a fixture FAILS here, so the IR shape cannot be
/// frozen with an un-gated op.
#[test]
fn every_op_variant_has_a_fixture() {
    // The closed Op discriminant set, derived from the schema the way
    // `zeroship-migrate`'s own `op_ir_schema::op_variant_names_from_schema` does.
    let schema_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../zeroship-migrate/op-ir.schema.json");
    let schema: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&schema_path).expect("read op-ir.schema.json"),
    )
    .expect("parse op-ir.schema.json");
    let expected: BTreeSet<String> = schema
        .get("$defs")
        .and_then(|d| d.get("Op"))
        .and_then(|o| o.get("oneOf"))
        .and_then(|o| o.as_array())
        .expect("Op oneOf branches")
        .iter()
        .filter_map(|b| {
            b.get("properties")
                .and_then(|p| p.get("op"))
                .and_then(|t| t.get("const"))
                .and_then(|c| c.as_str())
                .map(str::to_string)
        })
        .collect();
    assert_eq!(
        expected.len(),
        48,
        "the closed Op set has 48 variants (29 portable/core enum/domain/sequence/trigger/view/comment/runtime-option variants + 19 @zeroship/migrate/pg vendor)"
    );

    // The union of op discriminants across all recorded fixtures.
    let mut covered: BTreeSet<String> = BTreeSet::new();
    for stem in fixture_stems() {
        let mig_src = std::fs::read_to_string(fixtures_dir().join(format!("{stem}.mig.js")))
            .unwrap_or_else(|e| panic!("read {stem}.mig.js: {e}"));
        let recorded = record_migration_to_json_unsandboxed(&mig_src, OWNER, &stem)
            .unwrap_or_else(|e| panic!("record {stem}: {e}"));
        let ir: MigrationIr = serde_json::from_str(&recorded).expect("recorded IR");
        for op in &ir.ops {
            let v = serde_json::to_value(op).expect("op -> value");
            if let Some(tag) = v.get("op").and_then(|t| t.as_str()) {
                covered.insert(tag.to_string());
            }
        }
    }

    let missing: Vec<&String> = expected.difference(&covered).collect();
    assert!(
        missing.is_empty(),
        "every Op variant must have at least one corpus fixture; un-gated variants: {missing:?}"
    );
}
