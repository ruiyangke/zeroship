//! **The descriptor CARRIES the physical storage facts, it does not imply them.**
//!
//! One declared field can occupy more than one physical database object. When this
//! module was written, the only consumer that needed those names re-derived them by
//! string formatting - `format!("{col}_masked")` appeared at eight independent sites
//! listed in `docs/reviews/2026-08-27-descriptor-specification.md` section 1.4 - and
//! a name derived at eight sites is eight chances to disagree with the one emitter
//! that actually created the column.
//!
//! **The raw column's half of that is closed as of 2026-09-04.** The data plane's
//! three CRUD consumers (the write relocation, the read strip and the unmask SELECT)
//! read `storage.rawColumn` through `zeroship_data_sql::compile::declared_raw_column`
//! instead of formatting it. What still derives is the pair of backend
//! introspectors, and no descriptor can serve them: introspection reports what a
//! database contains, and the catalog records no mask-to-raw pairing to report.
//!
//! These arms pin the emitter side of that fix: every field of every rendered
//! descriptor names the column a default projection reads, the column holding the
//! authoritative value when the two differ, the raw column's read-surface
//! capabilities, and any auxiliary physical object the field owns.
//!
//! The arms deliberately assert on the SERIALIZED `schema.runtime.json` rather than
//! on the typed struct behind it. A capability flag that a reader supplies as a
//! `Default` looks identical to one the producer emitted, from inside Rust; only the
//! bytes can tell them apart, and the bytes are what the TypeScript consumer sees.

use super::*;
use crate::render::declarative::{CollectionDescriptor, FieldDescriptor};
use crate::test_fixtures::{POSTGRES, SQLITE};
use serde_json::json;

/// Render one collection to the runtime descriptor and hand back the parsed JSON.
///
/// `no_inject` rather than the confined charter: the platform charter injects seven
/// system columns into every table, and an assertion about "the fields of this
/// collection" reads far better over the two the test declared.
fn descriptor_for(
    fields: Vec<FieldDescriptor>,
    dialect: &zeroship_migrate_ir::dialect::DialectId,
) -> Value {
    let effective = crate::test_fixtures::no_inject("app");
    let descriptors = [CollectionDescriptor {
        name: "people".to_string(),
        owner_app: "app_test".to_string(),
        fields,
        indexes: Vec::new(),
        runtime_options: Default::default(),
    }];
    let artifacts = render_artifacts_from_descriptors(
        crate::test_fixtures::VENDORS,
        &descriptors,
        dialect,
        DEFAULT_PROJECT_SCHEMA,
        &effective,
    )
    .expect("descriptors render");
    serde_json::from_str(&artifacts.runtime_json).expect("runtime descriptor is valid JSON")
}

fn plain(name: &str) -> FieldDescriptor {
    FieldDescriptor {
        name: name.to_string(),
        ty: "string".to_string(),
        ..Default::default()
    }
}

fn masked(name: &str) -> FieldDescriptor {
    FieldDescriptor {
        mask: Some(json!({ "kind": "last4", "classification": "pci" })),
        ..plain(name)
    }
}

fn encrypted_and_masked(name: &str) -> FieldDescriptor {
    FieldDescriptor {
        encrypted: Some(true),
        mask: Some(json!({ "kind": "full", "classification": "pii" })),
        ..plain(name)
    }
}

/// A masked field occupies TWO columns, and the descriptor names both.
///
/// The three raw capability flags declare a policy, not just an observation: they
/// say the raw column is not reachable through the creator-facing read surface.
/// Before the 2026-08-28 storage flip the field's own column held plaintext and
/// `build_where` (which takes no schema hint) could reach it directly through an
/// ordinary `find({ ssn: x })` - an unaudited binary search over a value the caller
/// could not read (specification section 4.3). After the flip the field's own
/// column holds the mask and the raw column's `__zs_raw__` name is refused by
/// every inbound identifier surface, so an ordinary filter can no longer name it at
/// all; the flags remain the declared contract a consumer reads instead of
/// re-deriving the naming rule.
#[test]
fn a_masked_field_records_both_of_its_physical_columns() {
    let value = descriptor_for(vec![masked("ssn")], &POSTGRES);
    let storage = &value["collections"]["people"]["fields"]["ssn"]["storage"];

    let raw = crate::schema::query::raw_column_for_field(
        "ssn",
        &json!({ "mask": { "kind": "last4", "classification": "pci" } }),
    )
    .expect("a last4 mask declares a raw column");

    assert_eq!(
        storage["valueColumn"], "ssn",
        "after the storage flip a default projection reads the field's own column, \
         which now holds the mask: {value}"
    );
    assert_eq!(
        storage["rawColumn"], raw,
        "the authoritative value lives in the raw column named by raw_column_for_field: {value}"
    );
    assert_eq!(storage["rawFilterable"], false, "{value}");
    assert_eq!(storage["rawSortable"], false, "{value}");
    assert_eq!(storage["rawProjectable"], false, "{value}");
}

/// An ordinary field occupies exactly one column, so there is no raw sibling to name.
///
/// `valueColumn` is still emitted. A consumer that must never format a column name
/// needs a total function, and an absent `valueColumn` would put the `format!` back.
#[test]
fn an_ordinary_field_records_one_column_and_no_raw_sibling() {
    let value = descriptor_for(vec![plain("nickname")], &POSTGRES);
    let storage = &value["collections"]["people"]["fields"]["nickname"]["storage"];

    assert_eq!(storage["valueColumn"], "nickname", "{value}");
    assert!(
        storage.get("rawColumn").is_none(),
        "an unmasked, unencrypted field has no second physical column: {value}"
    );
    assert!(
        storage.get("rawFilterable").is_none(),
        "a capability flag about a column that does not exist is not state: {value}"
    );
}

/// An encrypted field's CIPHERTEXT is the authoritative value, so it is what
/// `rawColumn` names.
///
/// This is the arm that pins WHERE the ciphertext physically lives - `rawColumn`
/// when present, the field's own column otherwise. It is deliberately NOT the arm
/// the AEAD tag depends on: `canonical_aad` binds the LOGICAL field name (the
/// schema key), not this physical column, so a future storage move stays a rename
/// rather than a re-encrypt. An earlier draft of this comment claimed the AAD
/// bound the physical column instead; see the correction atop `FieldStorage` in
/// `gen_types.rs` for why that was false and why "fixing" the AAD to match it would
/// have destroyed every ciphertext already written under the logical name.
#[test]
fn an_encrypted_and_masked_field_puts_the_ciphertext_column_in_raw_column() {
    let value = descriptor_for(vec![encrypted_and_masked("card")], &POSTGRES);
    let field = &value["collections"]["people"]["fields"]["card"];
    let storage = &field["storage"];

    assert!(
        field.get("encrypted").and_then(serde_json::Value::as_bool) == Some(true),
        "fixture must actually be encrypted: {value}"
    );
    assert_eq!(
        storage["rawColumn"],
        crate::schema::query::raw_column_name("card"),
        "the ciphertext lives in the raw column: {value}"
    );
    assert_ne!(
        storage["valueColumn"], storage["rawColumn"],
        "the projected column and the ciphertext column are different objects: {value}"
    );
}

/// An encrypted field that opts out of masking with `kind: "none"` has ONE column,
/// and the ciphertext lives in it.
///
/// Pinned because it is the case that makes the AAD rule total: with no `rawColumn`
/// the consumer must fall back to the field's own column, and if this arm emitted a
/// `rawColumn` anyway the fallback would be dead code that never got exercised.
///
/// The `classification` is not decoration. The descriptor producer reads it with
/// `.get("classification").and_then(as_str)?` (`render/fold.rs:5819-5821`), so a mask
/// facet without one is dropped WHOLE - and on an encrypted column the fail-safe
/// `{ full, pii }` auto-mask then reapplies (`render/lower.rs:9591`). The first draft of
/// this arm authored a bare `{ kind: "none" }` and measured a `token_masked` sibling it
/// had just asked not to exist. The direction is safe (more masking, never less), but
/// the fixture has to say what the pipeline actually reads.
#[test]
fn an_encrypted_field_that_opts_out_of_masking_keeps_one_column() {
    let field = FieldDescriptor {
        mask: Some(json!({ "kind": "none", "classification": "pii" })),
        ..encrypted_and_masked("token")
    };
    let value = descriptor_for(vec![field], &POSTGRES);
    let storage = &value["collections"]["people"]["fields"]["token"]["storage"];

    assert_eq!(storage["valueColumn"], "token", "{value}");
    assert!(
        storage.get("rawColumn").is_none(),
        "`kind: none` emits no sibling, so there is no second column: {value}"
    );
}

/// The read-surface capabilities are EMITTED on every field, not inferred by the
/// reader from the field's presence in the map.
///
/// Today the consumer infers all four from one membership test
/// (`validate_read_identifier` applies the same check to `select` and `orderBy`), so
/// these values are what that inference already produces. The point is not the value;
/// it is that narrowing one later becomes a producer change rather than a Rust edit.
#[test]
fn every_field_emits_its_read_surface_capabilities() {
    let value = descriptor_for(vec![plain("nickname"), masked("ssn")], &POSTGRES);
    let fields = value["collections"]["people"]["fields"]
        .as_object()
        .expect("fields is an object");
    assert!(!fields.is_empty(), "fixture declares fields: {value}");

    for (name, def) in fields {
        for flag in ["readable", "filterable", "sortable", "projectable"] {
            assert_eq!(
                def.get(flag),
                Some(&Value::Bool(true)),
                "field {name} must carry an explicit `{flag}`: {value}"
            );
        }
    }
}

/// The flags survive the round trip through the serialized artifact.
///
/// A `#[serde(default)]` on the reader would make an ABSENT `false` and an EMITTED
/// `false` indistinguishable from inside Rust. Re-parsing the bytes and looking for
/// the KEY is the only test that can tell them apart, which is why this arm asserts
/// on `get(..).is_some()` and not on the value.
#[test]
fn the_capability_flags_are_present_in_the_bytes_not_supplied_by_the_reader() {
    let value = descriptor_for(vec![masked("ssn")], &POSTGRES);
    let reserialized = serde_json::to_string(&value).expect("descriptor reserializes");
    let reparsed: Value = serde_json::from_str(&reserialized).expect("descriptor reparses");
    let storage = &reparsed["collections"]["people"]["fields"]["ssn"]["storage"];

    for flag in ["rawFilterable", "rawSortable", "rawProjectable"] {
        assert!(
            storage.get(flag).is_some(),
            "`{flag}` must be a key in the emitted JSON, not a reader default: {reparsed}"
        );
        assert_eq!(storage[flag], false, "{reparsed}");
    }
    assert!(storage.get("valueColumn").is_some(), "{reparsed}");
    assert!(storage.get("rawColumn").is_some(), "{reparsed}");
}

/// The descriptor's `valueColumn` / `rawColumn` are the DDL emitter's OWN names,
/// not a second spelling of the same convention.
///
/// This is the arm that outlives the current physical layout: it reads both names
/// from `raw_column_for_field` rather than hardcoding either, so a storage change
/// only has to move the projection here to keep this arm green. The 2026-08-28
/// storage flip already exercised that promise once - `valueColumn` was the
/// emitted `<col>_masked` sibling before the flip and is the field's own column
/// now, while `rawColumn` moved the other way, from the field's own column to
/// `raw_column_name(field)` - and this arm needed no shape change, only the
/// renamed source function, which is exactly the coupling the eight `format!`
/// sites never had.
#[test]
fn the_recorded_columns_are_the_ddl_emitters_own_names() {
    let value = descriptor_for(vec![masked("ssn"), plain("nickname")], &POSTGRES);
    let fields = value["collections"]["people"]["fields"]
        .as_object()
        .expect("fields is an object");

    for (name, def) in fields {
        let storage = &def["storage"];
        match crate::schema::query::raw_column_for_field(name, def) {
            Some(raw) => {
                assert_eq!(
                    storage["valueColumn"], name.as_str(),
                    "field {name}: after the flip valueColumn is the field's own name: {value}"
                );
                assert_eq!(
                    storage["rawColumn"], raw,
                    "field {name} must record the emitter's own raw column name: {value}"
                );
            }
            None => {
                assert_eq!(storage["valueColumn"], name.as_str(), "{value}");
                assert!(storage.get("rawColumn").is_none(), "{value}");
            }
        }
    }
}

/// Vector storage describes the column migrations actually create.
#[test]
fn vector_storage_is_the_base_column_on_each_target() {
    for dialect in [&SQLITE, &POSTGRES] {
        let embedding = FieldDescriptor {
            name: "embedding".to_string(),
            ty: "vector".to_string(),
            vector_dims: Some(3),
            vector_metric: Some("cosine".to_string()),
            ..Default::default()
        };
        let value = descriptor_for(vec![embedding], dialect);
        assert_eq!(
            value["collections"]["people"]["fields"]["embedding"]["storage"],
            json!({"valueColumn": "embedding"}),
            "{value}"
        );
    }
}

/// The descriptor announces itself as v2.
///
/// The bump is load-bearing rather than cosmetic: v2 GUARANTEES a `storage` block on
/// every field, and a consumer that stops formatting column names depends on that
/// guarantee. A committed v1 artifact does not carry it, so the reader must be able to
/// refuse one instead of silently serving a descriptor with the facts missing.
#[test]
fn the_descriptor_announces_version_two() {
    let value = descriptor_for(vec![plain("nickname")], &POSTGRES);
    assert_eq!(value["version"], 2, "{value}");
}

/// The TYPED storage block round-trips through the artifact bytes.
///
/// The arms above assert on `Value` because that is the only vantage point from which
/// an emitted `false` and a reader-supplied `false` look different. This one closes the
/// other half: a consumer will read this back into [`FieldStorage`], and every field
/// carrying `skip_serializing_if` needs a `serde(default)` beside it or that read fails
/// outright on the ordinary-field shape. Serializing and deserializing the same value is
/// the cheapest way to keep the two attributes in step.
#[test]
fn the_typed_storage_block_survives_a_serde_round_trip() {
    let value = descriptor_for(vec![masked("ssn"), plain("nickname")], &POSTGRES);
    let fields = value["collections"]["people"]["fields"]
        .as_object()
        .expect("fields is an object");

    let mut seen_raw = 0usize;
    let mut seen_plain = 0usize;
    for (name, def) in fields {
        let storage: FieldStorage = serde_json::from_value(def["storage"].clone())
            .unwrap_or_else(|e| panic!("field {name} storage deserializes: {e}: {value}"));
        let reserialized =
            serde_json::to_value(&storage).expect("FieldStorage reserializes");
        assert_eq!(
            reserialized, def["storage"],
            "field {name} storage is not stable across the round trip: {value}"
        );
        // After the flip valueColumn is the field's own name unconditionally -
        // masked and plain fields no longer diverge on this point.
        assert_eq!(storage.value_column, *name, "{value}");
        match &storage.raw_column {
            Some(raw) => {
                seen_raw += 1;
                assert_eq!(*raw, crate::schema::query::raw_column_name(name));
                assert_eq!(storage.raw_filterable, Some(false));
                assert_eq!(storage.raw_sortable, Some(false));
                assert_eq!(storage.raw_projectable, Some(false));
            }
            None => {
                seen_plain += 1;
                assert_eq!(storage.raw_filterable, None);
            }
        }
    }
    assert_eq!(seen_raw, 1, "the masked field must exercise the raw arm");
    assert_eq!(seen_plain, 1, "the plain field must exercise the no-raw arm");
}
