//! An operation referencing a column an earlier `dropColumn` removed is refused.
//!
//! The column-level sibling of the table-level check in
//! `op_after_rename_targets_old_name.rs`, which recorded this case as measured and
//! deliberately unfixed because it needs different machinery: that walk reads ONE
//! name per op from `touched_table`, while this needs every column an op
//! REFERENCES.
//!
//! Measured before the fix:
//!
//!     ALTER TABLE "prj_ir"."a" DROP COLUMN "v"
//!     CREATE INDEX IF NOT EXISTS "ix" ON "prj_ir"."a" ("v")
//!
//! The index names a column that is gone, and PostgreSQL fails it mid-migration
//! with `column "v" does not exist`.
//!
//! COVERAGE IS BOUNDED AND SAID SO OUT LOUD. This checks the places a column name
//! is a plain identifier in the IR: the `column` field of the alter-column family,
//! `createIndex` elements, and the column lists of UNIQUE and foreign-key
//! constraints.
//!
//! EXPRESSIONS ARE COVERED TOO, in `expr_references_a_dropped_column.rs`. This
//! fixture once recorded them as out of reach on the grounds that they are trees
//! rather than identifier lists. That premise was right and the conclusion was
//! wrong: `Expr` is a closed AST and the renderer already carries an exhaustive
//! walk over it, so only the wiring was missing. A backfill `set` value is the
//! one expression site still unreached.

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

const TABLE: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true},{"name":"w","type":"int","nullable":true}],"primaryKey":["c0"]}"#;

fn verdict(tail: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{TABLE},{tail}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, Dialect::Postgres).map_err(|e| format!("{}: {}", e.code, e.reason))
}

#[test]
fn an_index_on_a_dropped_column_is_refused() {
    let refusal = verdict(
        r#"{"op":"dropColumn","table":"a","column":"v"},{"op":"createIndex","name":"ix","table":"a","columns":[{"kind":"column","name":"v"}]}"#,
    )
    .expect_err("the index names a column the drop removed");
    assert!(
        refusal.to_lowercase().contains("drop"),
        "the refusal must point at the drop that removed it: {refusal}"
    );
}

/// Assert the refusal names THIS op, not merely that a column went missing.
///
/// Both sites here produced an identical message until the rule was taught to
/// name its operation, so the setColumnNotNull test was satisfied by the
/// addConstraint refusal and neither could be pinned.
fn expect_named_op_refusal(ops: &str, op_name: &str, what: &str) {
    let refusal = verdict(ops).expect_err(what);
    let expected = format!("this {op_name} names column");
    assert!(
        refusal.contains(&expected),
        "{expected:?} is missing, so a sibling refusal satisfies this test: {refusal}"
    );
}

#[test]
fn altering_a_dropped_column_is_refused() {
    expect_named_op_refusal(
        r#"{"op":"dropColumn","table":"a","column":"v"},{"op":"setColumnNotNull","table":"a","column":"v"}"#,
        "setColumnNotNull",
        "setColumnNotNull names a column the drop removed",
    );
}

#[test]
fn a_unique_constraint_over_a_dropped_column_is_refused() {
    expect_named_op_refusal(
        r#"{"op":"dropColumn","table":"a","column":"v"},{"op":"addConstraint","table":"a","constraint":{"kind":{"kind":"unique","columns":["v"]}}}"#,
        "addConstraint",
        "the constraint names a column the drop removed",
    );
}

#[test]
fn dropping_a_column_and_re_adding_it_is_still_allowed() {
    // THE CONTROL THAT SHAPES THE CHECK, exactly as it did at table level:
    // drop-then-re-add is how a column's type is changed, and the re-added column
    // must be usable afterwards.
    verdict(
        r#"{"op":"dropColumn","table":"a","column":"v"},{"op":"addColumn","table":"a","column":"v","type":"text","nullable":true},{"op":"createIndex","name":"ix","table":"a","columns":[{"kind":"column","name":"v"}]}"#,
    )
    .expect("drop, re-add, then use is a real migration pattern");
}

#[test]
fn an_index_on_a_different_column_is_still_allowed() {
    verdict(
        r#"{"op":"dropColumn","table":"a","column":"v"},{"op":"createIndex","name":"ix","table":"a","columns":[{"kind":"column","name":"w"}]}"#,
    )
    .expect("dropping one column must not block indexing another");
}

#[test]
fn dropping_a_column_of_a_different_table_does_not_block() {
    // The vacated set is per TABLE: dropping `b.v` says nothing about `a.v`.
    let bytes = format!(
        r#"{{"ir_version":1,"name":"n","ops":[{TABLE},{{"op":"createTable","name":"b","columns":[{{"name":"c0","type":"int","nullable":false}},{{"name":"v","type":"int","nullable":true}}],"primaryKey":["c0"]}},{{"op":"dropColumn","table":"b","column":"v"}},{{"op":"createIndex","name":"ix","table":"a","columns":[{{"kind":"column","name":"v"}}]}}]}}"#
    );
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, Dialect::Postgres).expect("a drop on another table must not block this one");
}
