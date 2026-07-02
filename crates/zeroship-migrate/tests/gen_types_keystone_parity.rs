//! **Migration-first P2b §6(b) — THE KEYSTONE.**
//!
//! A schema authored DECLARATIVELY (`eval_schema_to_ir` → `CollectionDescriptor` →
//! `descriptor_to_sdk_schema`) and THE MIGRATION IT GENERATES
//! (`descriptors_to_create_ops` → ops → `fold_to_field_defs`) MUST produce
//! BYTE-IDENTICAL per-collection wire-`FieldDef` maps. This proves the
//! author→generate→fold chain is LOSSLESS over exactly the facets the `@zeroship/db`
//! type inference consumes.
//!
//! WHY this is RED pre-P2b: `descriptors_to_create_ops` (the descriptor→ops producer
//! that threads idPrefix/vectorMetric/encrypted/ref) did not exist before P2b — the
//! snapshot-sourced `generate_ops` could not carry those facets (`col_type_for_data_type`
//! fail-closes on vector/encrypted; `id_prefix: None`). Without the producer, this
//! test cannot even be written, and a naive snapshot round-trip would DROP every
//! goodie → the maps would diverge. CHECK-borne enum/min/max facets are outside P0
//! until the Expr->SQL renderer lands.
//!
//! FAITHFUL: the declarative side is the REAL V8-evaluated `@zeroship/db` schema; the
//! generated side runs the REAL `descriptors_to_create_ops` producer + the REAL
//! `fold_to_field_defs` recovery seam — no shims.

use zeroship_migrate::frontend::eval_schema_to_ir;
use zeroship_migrate::render::declarative::{descriptor_to_sdk_schema, CollectionDescriptor};
use zeroship_migrate::{descriptors_to_create_ops, fold_to_field_defs, SqlDialect};

const KEYSTONE: &str = include_str!("fixtures/keystone_schema.js");
const SCHEMA: &str = "public";

/// The declarative side: each collection's `descriptor_to_sdk_schema` Value.
fn declarative_field_defs(
    descriptors: &[CollectionDescriptor],
) -> std::collections::BTreeMap<String, serde_json::Value> {
    descriptors
        .iter()
        .map(|d| (d.name.clone(), descriptor_to_sdk_schema(d)))
        .collect()
}

fn generated_authored_subset(
    generated_def: &serde_json::Value,
    authored_def: &serde_json::Value,
) -> serde_json::Value {
    let generated = generated_def.as_object().expect("generated field map");
    let authored = authored_def.as_object().expect("authored field map");
    let mut subset = serde_json::Map::new();
    for key in authored.keys() {
        let value = generated
            .get(key)
            .unwrap_or_else(|| panic!("generated side missing authored field {key}"));
        subset.insert(key.clone(), value.clone());
    }
    serde_json::Value::Object(subset)
}

#[test]
fn keystone_authored_vs_generated_field_defs_are_byte_identical() {
    let descriptors = eval_schema_to_ir(KEYSTONE, "app_keystone").expect("eval keystone schema");
    assert!(!descriptors.is_empty(), "keystone schema has collections");

    // Side A — authored declaratively.
    let authored = declarative_field_defs(&descriptors);

    // Side B — the migration the descriptors GENERATE, folded-and-recovered.
    let ops = descriptors_to_create_ops(&descriptors).expect("descriptor → ops producer");
    let generated =
        fold_to_field_defs(&ops, SqlDialect::Postgres, SCHEMA).expect("fold-and-recover");

    // BYTE-IDENTICAL per collection for the authored fields. Slice 6 makes the
    // produced createTable carry the resolved confined system-field prefix too;
    // those policy-injected fields are asserted by the fold unit tests and the
    // gen-types CLI golden, while this keystone stays focused on declared facet
    // fidelity.
    assert_eq!(
        authored.keys().collect::<Vec<_>>(),
        generated.keys().collect::<Vec<_>>(),
        "the same collection set on both sides"
    );
    for (collection, authored_def) in &authored {
        let generated_def = generated
            .get(collection)
            .unwrap_or_else(|| panic!("generated side missing collection {collection}"));
        let a = serde_json::to_string_pretty(authored_def).unwrap();
        let b = serde_json::to_string_pretty(&generated_authored_subset(
            generated_def,
            authored_def,
        ))
        .unwrap();
        assert_eq!(
            a, b,
            "keystone parity DRIFT for collection `{collection}`:\n\
             --- authored (descriptor_to_sdk_schema) ---\n{a}\n\
             --- generated (descriptors_to_create_ops → fold_to_field_defs) ---\n{b}"
        );
    }
}

#[test]
fn keystone_recovers_declared_only_and_supported_facets() {
    // A focused assertion that the carried + lifted facets actually SURVIVE the
    // chain (so the byte-identity above isn't trivially "both dropped everything").
    let descriptors = eval_schema_to_ir(KEYSTONE, "app_keystone").expect("eval keystone schema");
    let ops = descriptors_to_create_ops(&descriptors).expect("producer");
    let generated = fold_to_field_defs(&ops, SqlDialect::Postgres, SCHEMA).expect("fold");

    let users = generated.get("users").expect("users folded");
    let docs = generated.get("docs").expect("docs folded");

    // idPrefix carried + recovered.
    assert_eq!(
        users["id"].get("idPrefix").and_then(|v| v.as_str()),
        Some("acct"),
        "the typed-id prefix survives author→generate→fold"
    );
    // ref brand recovered.
    assert_eq!(
        docs["authorId"].get("refTarget").and_then(|v| v.as_str()),
        Some("users"),
        "the FK brand survives"
    );
    // vector dims + metric recovered.
    assert_eq!(docs["embedding"].get("vectorDims").and_then(|v| v.as_i64()), Some(1536));
    assert_eq!(
        docs["embedding"].get("vectorMetric").and_then(|v| v.as_str()),
        Some("innerProduct"),
        "the declared vector metric survives"
    );

    // HIGH-1 + MED-1: a DEFAULT `t.encrypted()` recovers BOTH the kernel-default
    // encrypted facet AND the fail-safe auto-mask byte-identically — the goodie the
    // prior keystone fixture omitted so the chain only LOOKED lossless. Without the
    // recovery fix this DRIFTS (`encrypted: {}`, mask dropped).
    let token = &users["token"];
    assert_eq!(
        token.get("encrypted").and_then(|e| e.get("mode")).and_then(|v| v.as_str()),
        Some("randomised"),
        "the encrypted kernel-default mode survives author->generate->fold"
    );
    assert_eq!(
        token.get("encrypted").and_then(|e| e.get("keyId")).and_then(|v| v.as_str()),
        Some("default"),
        "the encrypted kernel-default keyId survives"
    );
    assert_eq!(
        token.get("encrypted").and_then(|e| e.get("wraps")).and_then(|v| v.as_str()),
        Some("string"),
        "the encrypted wraps inner-type survives"
    );
    assert_eq!(
        token.get("mask").and_then(|m| m.get("kind")).and_then(|v| v.as_str()),
        Some("full"),
        "the fail-safe auto-mask kind survives (MED-1: was silently dropped)"
    );
    assert_eq!(
        token.get("mask").and_then(|m| m.get("classification")).and_then(|v| v.as_str()),
        Some("pii"),
        "the fail-safe auto-mask classification survives"
    );
}
