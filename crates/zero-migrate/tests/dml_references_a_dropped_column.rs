//! An `update`, `delete` or `insert` naming a column dropped earlier in the same
//! migration is refused.
//!
//! The rest of the family F726 opened. That commit fixed `backfill`; the same gap
//! shape reaches every consumer of the resolving pass, because the gap is not in
//! any one op - it is BETWEEN two checks:
//!
//!   - `validate_ir_resolved` resolves these column references, but against the
//!     table's DECLARED column set, which still holds a column this migration
//!     dropped earlier;
//!   - the walk that TRACKS drops did not look at these references.
//!
//! Neither check is wrong on its own terms, which is why asking "is this check
//! correct?" of each one separately would never have found it.
//!
//! MEASURED. Both lowered, with the dropped column named in the emitted DML:
//!
//!     ALTER TABLE "public"."a" DROP COLUMN "v"
//!     UPDATE "public"."a" SET "w" = "v"
//!
//!     ALTER TABLE "public"."a" DROP COLUMN "v"
//!     DELETE FROM "public"."a" WHERE ("v" > ...)
//!
//! and PostgreSQL rejects that update with `column "v" does not exist`, measured
//! for the identical statement in the backfill fixture.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn verdict(ops: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, Dialect::Postgres, &[]).map_err(|e| format!("{}: {}", e.code, e.reason))
}

const A: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true},{"name":"w","type":"int","nullable":true}],"primaryKey":["c0"]}"#;
const DROP_V: &str = r#"{"op":"dropColumn","table":"a","column":"v"}"#;

#[test]
fn an_update_reading_a_dropped_column_is_refused() {
    let refusal = verdict(&format!(
        r#"{A},{DROP_V},{{"op":"update","table":"a","set":{{"w":{{"node":"colRef","name":"v"}}}}}}"#
    ))
    .expect_err("the update reads a column the drop removed");
    assert!(
        refusal.to_lowercase().contains("drop"),
        "the refusal must point at the drop: {refusal}"
    );
}

#[test]
fn an_update_writing_a_dropped_column_is_refused() {
    verdict(&format!(
        r#"{A},{{"op":"dropColumn","table":"a","column":"w"}},{{"op":"update","table":"a","set":{{"w":{{"node":"literal","value":1}}}}}}"#
    ))
    .expect_err("the update writes a column the drop removed");
}

#[test]
fn an_update_filtering_on_a_dropped_column_is_refused() {
    verdict(&format!(
        r#"{A},{DROP_V},{{"op":"update","table":"a","set":{{"w":{{"node":"literal","value":1}}}},"where":{{"node":"binOp","op":"gt","lhs":{{"node":"colRef","name":"v"}},"rhs":{{"node":"literal","value":0}}}}}}"#
    ))
    .expect_err("the WHERE clause reads a column the drop removed");
}

#[test]
fn a_delete_filtering_on_a_dropped_column_is_refused() {
    verdict(&format!(
        r#"{A},{DROP_V},{{"op":"delete","table":"a","where":{{"node":"binOp","op":"gt","lhs":{{"node":"colRef","name":"v"}},"rhs":{{"node":"literal","value":0}}}}}}"#
    ))
    .expect_err("the DELETE names a column the drop removed");
}

#[test]
fn an_insert_into_a_dropped_column_is_refused() {
    verdict(&format!(
        r#"{A},{DROP_V},{{"op":"insert","table":"a","columns":["c0","v"],"rows":[[1,2]]}}"#
    ))
    .expect_err("the insert names a column the drop removed");
}

// ---------------------------------------------------------------------------
// Controls.
// ---------------------------------------------------------------------------

#[test]
fn ordinary_dml_over_live_columns_is_still_allowed() {
    verdict(&format!(
        r#"{A},{{"op":"update","table":"a","set":{{"w":{{"node":"colRef","name":"v"}}}}}},{{"op":"insert","table":"a","columns":["c0","v"],"rows":[[1,2]]}}"#
    ))
    .expect("DML over live columns is the ordinary case");
}

#[test]
fn dml_after_dropping_an_unrelated_column_is_still_allowed() {
    verdict(&format!(
        r#"{A},{DROP_V},{{"op":"update","table":"a","set":{{"w":{{"node":"colRef","name":"c0"}}}}}}"#
    ))
    .expect("dropping one column must not block DML over others");
}

#[test]
fn dml_after_the_column_is_re_added_is_still_allowed() {
    verdict(&format!(
        r#"{A},{DROP_V},{{"op":"addColumn","table":"a","column":"v","type":"int","nullable":true}},{{"op":"update","table":"a","set":{{"w":{{"node":"colRef","name":"v"}}}}}}"#
    ))
    .expect("drop, re-add, then update is a real migration pattern");
}

#[test]
fn dml_on_a_different_table_is_unaffected() {
    verdict(&format!(
        r#"{A},{{"op":"createTable","name":"b","columns":[{{"name":"c0","type":"int","nullable":false}},{{"name":"v","type":"int","nullable":true}},{{"name":"w","type":"int","nullable":true}}],"primaryKey":["c0"]}},{DROP_V},{{"op":"update","table":"b","set":{{"w":{{"node":"colRef","name":"v"}}}}}}"#
    ))
    .expect("the vacated set is per table");
}
