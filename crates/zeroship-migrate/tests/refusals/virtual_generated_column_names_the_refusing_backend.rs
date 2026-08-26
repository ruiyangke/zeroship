//! A virtual generated column's refusal names the backend that actually refused.
//!
//! `render::declarative::generated_column_snapshot` is the declarative-path gate
//! for `stored: false`. It takes the target `DialectId`, asks it for
//! `Capability::VirtualGeneratedColumn`, and refuses when the answer is no. Its
//! refusal string was a `const`:
//!
//!     UNSUPPORTED { kind: "virtualColumn", dialect: "pg" }
//!
//! Two things were wrong with it and NEITHER was pinned by a test, which is how
//! it survived the vendor-spelling sweep:
//!
//!  1. `pg` is not a dialect id. The canonical id is `postgres`; `pg` is not even
//!     an accepted alias (`DialectId` deserialization refuses anything that is
//!     not the registered spelling).
//!  2. The literal ignored the `dialect` parameter the function was handed. Every
//!     backend lacking the capability reported itself as `pg`, so on any backend
//!     but PostgreSQL the message named the WRONG ENGINE. The refusing backend is
//!     PROVENANCE and has exactly one honest source: the id that was passed in.
//!
//! REACHABILITY. This is not a dead branch. The chain is entirely public:
//! `zeroship_migrate::desired_snapshot_for_dialect` -> `build_table_snapshot_impl`
//! -> `column_snapshot_for_field` -> `generated_column_snapshot`. It is a
//! DIFFERENT carrier from the IR op path (`model::validate` gates
//! `IrColumn::generated` and produces its own vendor-authored refusal), so the
//! validator's version of this rule does not stand in front of it. The first test
//! below drives the public entry point and reads the string back.
//!
//! The census below is deliberately not written as "assert PostgreSQL". It walks
//! the SHIPPING REGISTRY and exercises every backend that declares the capability
//! absent, so a fourth backend inherits the pin instead of re-opening the hole.
//! Today that set is exactly `{postgres}` — the floor assertion says so out loud,
//! because a walk over a discovered set passes vacuously when the set empties.

use zeroship_migrate::render::declarative::{
    desired_snapshot_for_dialect, CollectionDescriptor, FieldDescriptor,
};
use zeroship_migrate::{shipping_backends, BinaryOp, Capability, DialectId, Expr, GeneratedCol};
use zeroship_migrate_postgres::DIALECT as POSTGRES;
use zeroship_migrate_sqlite::DIALECT as SQLITE;

use crate::support;

const PROJECT: &str = "app";
const APP: &str = "app_test";

/// Exactly one backend ships without `VirtualGeneratedColumn` (PostgreSQL). A
/// walk that finds NONE is an instrument failure, not a pass.
const REFUSING_BACKEND_FLOOR: usize = 1;

/// `qty * unit_cents`, declared VIRTUAL (`stored: false`) — the only facet under
/// test.
fn virtual_total() -> GeneratedCol {
    GeneratedCol {
        expr: Expr::BinOp {
            op: BinaryOp::Mul,
            lhs: Box::new(Expr::col("qty")),
            rhs: Box::new(Expr::col("unit_cents")),
        },
        stored: false,
    }
}

fn int_field(name: &str) -> FieldDescriptor {
    FieldDescriptor {
        name: name.to_string(),
        ty: "int".to_string(),
        ..Default::default()
    }
}

/// A table whose third column is a VIRTUAL generated column.
fn descriptors(stored: bool) -> Vec<CollectionDescriptor> {
    let mut total = int_field("total_cents");
    total.generated = Some(GeneratedCol {
        stored,
        ..virtual_total()
    });
    vec![CollectionDescriptor {
        name: "line_items".to_string(),
        owner_app: APP.to_string(),
        fields: vec![int_field("qty"), int_field("unit_cents"), total],
        indexes: vec![],
        runtime_options: Default::default(),
    }]
}

/// The public declarative entry point, reduced to its verdict.
fn desired(dialect: &DialectId, stored: bool) -> Result<(), String> {
    desired_snapshot_for_dialect(
        zeroship_migrate::shipping_vendors(),
        PROJECT,
        &descriptors(stored),
        dialect,
        &support::confined_charter(),
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

#[test]
fn a_stored_generated_column_is_not_refused_anywhere() {
    // The precondition that makes the refusals below attributable to the VIRTUAL
    // facet rather than to anything else in this descriptor.
    for descriptor in shipping_backends().iter() {
        desired(&descriptor.id, true).unwrap_or_else(|refusal| {
            panic!(
                "a STORED generated column is portable, but {} refused: {refusal}",
                descriptor.id
            )
        });
    }
}

#[test]
fn a_virtual_generated_column_is_accepted_where_the_capability_is_declared() {
    // The other half of the precondition: the gate is the capability, not the
    // facet. SQLite declares `VirtualGeneratedColumn` and must not refuse.
    assert!(
        shipping_backends()
            .iter()
            .any(|d| d.id == SQLITE && d.capabilities.contains(Capability::VirtualGeneratedColumn)),
        "SQLite must declare VirtualGeneratedColumn for this control to mean anything"
    );
    desired(&SQLITE, false).expect("SQLite renders a VIRTUAL generated column");
}

#[test]
fn the_postgres_refusal_names_postgres_and_never_pg() {
    let refusal = desired(&POSTGRES, false)
        .expect_err("PostgreSQL has no VIRTUAL generated column and must refuse");

    // The whole message, so a drift in either half is visible in one place.
    assert!(
        refusal.contains(r#"UNSUPPORTED { kind: "virtualColumn", dialect: postgres }"#),
        "the refusal must name the canonical dialect id: {refusal}"
    );

    // The regression itself. `pg` is not a dialect id and must never come back —
    // quoted, bare, or in any other dress.
    assert!(
        !refusal.contains("pg"),
        "the refusal must not spell PostgreSQL `pg`: {refusal}"
    );
}

#[test]
fn every_backend_without_the_capability_names_itself() {
    // Provenance, not presentation: the message must be derived from the id that
    // was passed in, so it cannot be right for one backend and wrong for the rest.
    let mut refused = 0usize;
    for descriptor in shipping_backends().iter() {
        if descriptor
            .capabilities
            .contains(Capability::VirtualGeneratedColumn)
        {
            continue;
        }
        refused += 1;
        let Err(refusal) = desired(&descriptor.id, false) else {
            panic!(
                "{} declares no VirtualGeneratedColumn and must refuse a VIRTUAL column",
                descriptor.id
            );
        };
        let expected = format!(
            r#"UNSUPPORTED {{ kind: "virtualColumn", dialect: {} }}"#,
            descriptor.id.as_str()
        );
        assert!(
            refusal.contains(&expected),
            "{} must name ITSELF as the refusing backend, expected {expected:?}: {refusal}",
            descriptor.id
        );
    }

    // The census self-check. Without it this test passes when the loop body never
    // runs — a registry change that hides the refusing backend would read GREEN.
    assert!(
        refused >= REFUSING_BACKEND_FLOOR,
        "walked the shipping registry and found {refused} backend(s) without \
         VirtualGeneratedColumn, expected at least {REFUSING_BACKEND_FLOOR}"
    );
}
