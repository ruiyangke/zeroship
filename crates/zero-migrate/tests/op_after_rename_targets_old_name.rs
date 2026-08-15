//! An operation targeting a table name a `renameTable` has already moved away is
//! refused.
//!
//! Measured before the fix. This envelope was accepted and lowered:
//!
//!     ALTER TABLE "prj_ir"."a" RENAME TO "b"
//!     ALTER TABLE "prj_ir"."a" ADD COLUMN "n" text
//!
//! The second statement names a table that no longer exists by the time it runs.
//! PostgreSQL fails it with `relation "prj_ir.a" does not exist`, mid-migration.
//!
//! THE ENGINE ALREADY REFUSES THE COLUMN-LEVEL EQUIVALENT. A `renameColumn`
//! beside any other operation on the same table is rejected with "renameColumn
//! must be the only operation targeting table \"a\" in a migration". The same
//! class of mistake was caught for one rename operation and not the other, which
//! is the asymmetry this closes.
//!
//! TWO SHAPES MUST KEEP WORKING, and they are what makes this narrow:
//!
//!   - operating on the NEW name after the rename is the ordinary way to rename
//!     and then continue working with the table;
//!   - CREATING a fresh table under the old name is legitimate too — rename the
//!     original away, then define a new one in its place.
//!
//! Only an operation that expects the OLD name to still be there is wrong.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

const TABLE: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true}],"primaryKey":["c0"]}"#;

fn verdict(tail: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{TABLE},{tail}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, Dialect::Postgres, &[]).map_err(|e| format!("{}: {}", e.code, e.reason))
}

#[test]
fn an_operation_on_the_old_name_after_a_rename_is_refused() {
    let refusal = verdict(
        r#"{"op":"renameTable","table":"a","to":"b"},{"op":"addColumn","table":"a","column":"n","type":"text","nullable":true}"#,
    )
    .expect_err(
        "the second statement names a table the rename already moved away, so it \
         reaches the server as `ALTER TABLE a ADD COLUMN` against a relation that no \
         longer exists",
    );
    assert!(
        refusal.to_lowercase().contains("rename"),
        "the refusal must point at the rename, not merely at a missing table: {refusal}"
    );
}

#[test]
fn operating_on_the_new_name_after_a_rename_is_still_allowed() {
    // The ordinary pattern: rename, then keep working with the table.
    verdict(
        r#"{"op":"renameTable","table":"a","to":"b"},{"op":"addColumn","table":"b","column":"n","type":"text","nullable":true}"#,
    )
    .expect("continuing against the new name is the normal way to use a rename");
}

#[test]
fn creating_a_fresh_table_under_the_freed_name_is_still_allowed() {
    // Rename the original away, then define a new table in its place. A rule that
    // simply banned the old name for the rest of the envelope would refuse this.
    verdict(
        r#"{"op":"renameTable","table":"a","to":"b"},{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false}],"primaryKey":["c0"]}"#,
    )
    .expect("recreating a table under the freed name must remain allowed");
}

#[test]
fn operating_on_that_freed_name_after_recreating_it_is_allowed() {
    // And once recreated, the name is usable again — the check has to track the
    // name's state through the envelope, not just remember that it was renamed.
    verdict(
        r#"{"op":"renameTable","table":"a","to":"b"},{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false}],"primaryKey":["c0"]},{"op":"addColumn","table":"a","column":"n","type":"text","nullable":true}"#,
    )
    .expect("a recreated table can be operated on under its own name");
}

#[test]
fn a_rename_on_its_own_is_still_allowed() {
    verdict(r#"{"op":"renameTable","table":"a","to":"b"}"#).expect("a plain rename must pass");
}
