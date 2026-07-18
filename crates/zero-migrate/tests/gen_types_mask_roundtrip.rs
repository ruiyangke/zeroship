//! **Standalone `.mask()` ROUND-TRIPS through the op.* fold.**
//!
//! `fold_to_field_defs` has always recovered the ENCRYPTED auto-mask (the fail-safe
//! `{ full, pii }` every `t.encrypted()` column carries), because
//! that mask is the kernel default a `ColType::Encrypted` column unambiguously implies.
//!
//! A STANDALONE `.mask()` on a PLAINTEXT column (`t.string().mask({ kind: "last4" })`)
//! used to be DROPPED: the IR `IrColumn` had no `mask` field, and the offline op fold
//! has no live `zero-migrate:mask` COMMENT sentinel to read (the runtime's recovery source). So
//! the creator's `MaskedValue<string>` silently downgraded to `string`, AND the op lower
//! emitted no sentinel — so the RUNTIME (which DOES read the sentinel) never masked the
//! field either.
//!
//! BOTH gaps are closed by CARRYING the mask on `IrColumn.mask` (and `Op::AddColumn`):
//!   1. the producer `descriptors_to_create_ops` carries a standalone mask onto
//!      the produced `IrColumn`;
//!   2. the lower `ir_column_to_field` maps `IrColumn.mask` → `FieldDescriptor.mask`
//!      (explicit mask WINS over the encrypted auto-mask);
//!   3. so `fold_to_field_defs` recovers it AND the op lower emits the `zero-migrate:mask`
//!      sentinel + `_masked` sibling (closing the runtime masking-fidelity gap too).
//!
//! This test PINS the round-trip: a standalone mask authored on a plaintext column
//! SURVIVES descriptors → ops → fold and reappears on the recovered `FieldDef`. (The
//! live-PG `zero-migrate:mask` sentinel round-trip is pinned by `mask_addcol_pg.rs`.)

use zero_migrate::render::declarative::{
    descriptor_to_sdk_schema, CollectionDescriptor, FieldDescriptor,
};
use zero_migrate::{descriptors_to_create_ops, fold_to_field_defs, SqlDialect};

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
fn standalone_mask_on_plaintext_column_round_trips_through_the_fold() {
    let descriptor = CollectionDescriptor {
        name: "people".to_string(),
        owner_app: "app_gap".to_string(),
        fields: vec![standalone_masked_field()],
        indexes: Vec::new(),
        runtime_options: Default::default(),
    };

    // AUTHORED side: the declarative descriptor carries the mask.
    let authored = descriptor_to_sdk_schema(&descriptor);
    assert!(
        authored["ssn"].get("mask").is_some(),
        "the authored descriptor carries the standalone mask: {authored}"
    );

    // GENERATED side: produce ops + fold-and-recover.
    let effective = zero_migrate::zeroship_confined_ceiling();
    let ops = descriptors_to_create_ops(&[descriptor], SCHEMA, &effective).expect("producer");
    let generated =
        fold_to_field_defs(&ops, SqlDialect::Postgres, SCHEMA, &effective).expect("fold");
    let ssn = &generated["people"]["ssn"];

    // RECOVERED: the standalone mask now SURVIVES the op.* fold (carried on the IR,
    // re-derived into the FieldDescriptor, and emitted on the recovered FieldDef).
    let mask = ssn.get("mask").unwrap_or_else(|| {
        panic!("standalone .mask() must round-trip through the op.* fold; got: {ssn}")
    });
    assert_eq!(
        mask.get("kind").and_then(|v| v.as_str()),
        Some("last4"),
        "the recovered mask keeps its kind: {ssn}"
    );
    assert_eq!(
        mask.get("classification").and_then(|v| v.as_str()),
        Some("pii"),
        "the recovered mask keeps its classification: {ssn}"
    );
    // The column still types as its base scalar.
    assert_eq!(ssn.get("type").and_then(|v| v.as_str()), Some("string"));
}
