//! **Migration-first P2b — the standalone-`.mask()` KNOWN GAP (MED-1), made EXPLICIT.**
//!
//! `fold_to_field_defs` recovers the ENCRYPTED auto-mask (the fail-safe
//! `{ full, pii }` every `t.encrypted()` column carries — see the keystone), because
//! that mask is the kernel default a `ColType::Encrypted` column unambiguously
//! implies. But a STANDALONE `.mask()` on a PLAINTEXT column
//! (`t.string().mask({ kind: "last4" })`) has NO op.* carrier: the IR has no `mask`
//! field on `IrColumn` (by design — §4 lists mask as RECOVERED, not carried), and the
//! offline op fold has no rendered `__zsmask` COMMENT sentinel to read (the runtime's
//! recovery source). So a standalone mask is DROPPED through the author->generate->fold
//! chain — the creator's `MaskedValue<string>` downgrades to `string`.
//!
//! This test PINS that downgrade as the INTENTIONAL, documented behaviour (not a
//! silent surprise): closing it requires carrying a `mask` facet on the IR column,
//! tracked as a follow-up (see task: op.* AddColumn/CreateTable carry mask facet).
//! Until then the gate is: encrypted-auto-mask round-trips (keystone), standalone
//! mask is a known, asserted gap. If a future change starts round-tripping standalone
//! mask, THIS test flips RED and must be updated alongside the IR carry — exactly the
//! visible-exclusion the review demanded over a curated-around omission.

use zeroship_migrate::declarative::{descriptor_to_sdk_schema, CollectionDescriptor, FieldDescriptor};
use zeroship_migrate::{descriptors_to_create_ops, fold_to_field_defs, SqlDialect};

const SCHEMA: &str = "public";

/// A plaintext string column carrying ONLY a standalone `.mask()` (no encryption).
fn standalone_masked_field() -> FieldDescriptor {
    FieldDescriptor {
        name: "ssn".to_string(),
        ty: "string".to_string(),
        mask: Some(serde_json::json!({ "kind": "last4", "classification": "pii" })),
        ..Default::default()
    }
}

#[test]
fn standalone_mask_on_plaintext_column_is_a_known_gap_dropped_by_the_fold() {
    let descriptor = CollectionDescriptor {
        name: "people".to_string(),
        owner_app: "app_gap".to_string(),
        fields: vec![standalone_masked_field()],
        indexes: Vec::new(),
    };

    // AUTHORED side: the declarative descriptor DOES carry the mask.
    let authored = descriptor_to_sdk_schema(&descriptor);
    assert!(
        authored["ssn"].get("mask").is_some(),
        "the authored descriptor carries the standalone mask: {authored}"
    );

    // GENERATED side: produce ops + fold-and-recover.
    let ops = descriptors_to_create_ops(&[descriptor]).expect("producer");
    let generated = fold_to_field_defs(&ops, SqlDialect::Postgres, SCHEMA).expect("fold");
    let ssn = &generated["people"]["ssn"];

    // KNOWN GAP: the standalone mask is DROPPED (no op.* carrier, no offline sentinel).
    assert!(
        ssn.get("mask").is_none(),
        "standalone .mask() on a plaintext column is a known gap — DROPPED through the \
         op.* fold (no IR carrier / no offline sentinel). If this starts surviving, the \
         IR gained a mask carrier and this exclusion must be revisited. got: {ssn}"
    );
    // The column still types as its base scalar (not lost entirely).
    assert_eq!(ssn.get("type").and_then(|v| v.as_str()), Some("string"));
}
