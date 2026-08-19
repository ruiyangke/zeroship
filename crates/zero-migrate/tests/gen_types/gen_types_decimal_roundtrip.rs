//! **A fixed-precision decimal ROUND-TRIPS through the producer, not just outward.**
//!
//! The producer's own contract, stated in `render/fold.rs` above
//! `descriptors_to_create_ops`:
//!
//! ```text
//!   author (declarative)        descriptor_to_sdk_schema(descriptor)   ─┐
//!        │                                                              ├─ byte-identical
//!   descriptors_to_create_ops → ops → project_field_defs(fold(ops))   ─┘
//! ```
//!
//! The inverse that closes that loop is `token_to_col_type`, and `number` is the ONE
//! token it cannot map on the token alone: `col_type_to_token` spells both
//! `ColType::Double` and `ColType::Decimal { precision, scale }` as `number`, so the
//! `precision` facet beside it is what says WHICH type the descriptor meant. An inverse
//! that collapsed every `number` to `Double` would satisfy the byte-identity claim for
//! the TOKEN and break it for the facets - and a host that exported a
//! `t.numeric(20, 4)` column and fed it back as a manual source would get a float, with
//! the SQLite emitter then declaring it `REAL` and a rebuild copying its rows through a
//! binary double (`tests/fold_live/sqlite_decimal_rebuild_live.rs` measures that half
//! against a real database).
//!
//! This is a byte-identity pin between two projections of this repo, so it is NOT an
//! oracle for what any server stores. It is the round-trip half; the storage half is
//! adjudicated live in the file named above.

use crate::support;

use zero_migrate::render::declarative::{
    descriptor_to_sdk_schema, CollectionDescriptor, FieldDescriptor,
};
use zero_migrate::render::fold::single_fold;
use zero_migrate::{descriptors_to_create_ops, SqlDialect};

const SCHEMA: &str = "public";

fn ledger_descriptor() -> CollectionDescriptor {
    CollectionDescriptor {
        name: "ledger".to_string(),
        owner_app: "app_decimal".to_string(),
        fields: vec![
            FieldDescriptor {
                name: "amount".to_string(),
                ty: "number".to_string(),
                precision: Some(20),
                scale: Some(4),
                ..Default::default()
            },
            // The control that makes the assertion mean something: the SAME token with
            // no facet must stay a float on both sides. Without it a producer that
            // stamped `precision` onto every `number` would pass.
            FieldDescriptor {
                name: "rate".to_string(),
                ty: "number".to_string(),
                ..Default::default()
            },
        ],
        indexes: Vec::new(),
        runtime_options: Default::default(),
    }
}

#[test]
fn a_fixed_precision_decimal_survives_descriptors_to_ops_and_back() {
    let descriptor = ledger_descriptor();

    // AUTHORED side.
    let authored = descriptor_to_sdk_schema(&descriptor);
    assert_eq!(
        authored["amount"],
        serde_json::json!({ "type": "number", "precision": 20, "scale": 4 }),
        "the authored descriptor carries both parameters beside the shared token"
    );
    assert_eq!(
        authored["rate"],
        serde_json::json!({ "type": "number" }),
        "and a plain float carries neither"
    );

    // GENERATED side: descriptors -> ops -> fold -> recovered FieldDef.
    let effective = support::confined_charter();
    let ops = descriptors_to_create_ops(&[descriptor], SCHEMA, &effective).expect("producer");
    let generated = single_fold::fold(&ops, SqlDialect::Postgres, SCHEMA, &effective)
        .map(|folded| folded.project_field_defs())
        .expect("fold");

    assert_eq!(
        generated["ledger"]["amount"], authored["amount"],
        "the decimal came back byte-identical. A bare `{{\"type\":\"number\"}}` here is \
         the inverse having collapsed it to `ColType::Double` on the token alone - the \
         column is now an IEEE-754 float that no later projection can tell from the \
         decimal the author wrote."
    );
    assert_eq!(
        generated["ledger"]["rate"], authored["rate"],
        "and the float came back a float"
    );
}
