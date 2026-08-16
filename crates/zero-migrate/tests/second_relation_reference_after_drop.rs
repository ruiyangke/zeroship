//! An op naming a SECOND relation that an earlier op removed is refused.
//!
//! F723 established the shape: the use-after-drop walk compares ONE name per op,
//! via `touched_table()`, so a relation an op names IN ADDITION to its own target
//! is asked about by nothing. That commit fixed one instance - a partition's
//! parent - and this closes the rest of the family.
//!
//! MEASURED. Each lowered, and a live server rejected each:
//!
//!     DROP TABLE a;
//!     CREATE VIEW vw AS SELECT c0 FROM a          relation "sn.a" does not exist
//!
//!     DROP TABLE b;
//!     ALTER TABLE a ADD CONSTRAINT fk FOREIGN KEY (c0) REFERENCES b (c0)
//!                                                 relation "sn.b" does not exist
//!
//! plus the parent of `attachPartition`, `detachPartition` and `dropPartition`,
//! which `createPartition` alone had covered.
//!
//! FORWARD REFERENCES STAY LEGAL, and this is the line that keeps the check
//! honest. It refuses only a name this migration VACATED - dropped or renamed
//! away - never one that merely has not been created yet. A foreign key pointing
//! at a table defined LATER in the same envelope is a real pattern the engine
//! supports by deferring the constraint, and a control below pins it.
//!
//! RAW VIEW BODIES ARE OUT: a raw body is opaque SQL text and parsing it is a
//! different job with its own failure modes. Structured queries carry their
//! source tables as data, so those cost nothing.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn verdict(ops: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, Dialect::Postgres, &[]).map_err(|e| format!("{}: {}", e.code, e.reason))
}

const A: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false}],"primaryKey":["c0"]}"#;
const B: &str = r#"{"op":"createTable","name":"b","columns":[{"name":"c0","type":"int","nullable":false}],"primaryKey":["c0"]}"#;
const PARENT: &str = r#"{"op":"createTable","name":"par","columns":[{"name":"c0","type":"int","nullable":false}],"primaryKey":["c0"],"partitionBy":{"kind":"range","columns":["c0"]}}"#;
const P1: &str = r#"{"op":"createPartition","name":"p1","of":"par","bounds":{"kind":"range","from":[{"kind":"int","value":0}],"to":[{"kind":"int","value":10}]}}"#;

fn view_from(source: &str) -> String {
    format!(
        r#"{{"op":"createView","name":"vw","query":{{"kind":"structured","select":{{"from":{{"name":"{source}"}},"projection":[{{"kind":"colRef","name":"c0"}}]}}}}}}"#
    )
}
fn fk_to(target: &str) -> String {
    format!(
        r#"{{"op":"addConstraint","table":"a","constraint":{{"name":"fk","kind":{{"kind":"fk","columns":["c0"],"referencesTable":"{target}","referencesColumns":["c0"]}}}}}}"#
    )
}

#[test]
fn a_view_reading_a_dropped_table_is_refused() {
    let refusal = verdict(&format!(
        r#"{A},{{"op":"dropTable","table":"a"}},{}"#,
        view_from("a")
    ))
    .expect_err("the view's source table was dropped earlier");
    assert!(
        refusal.to_lowercase().contains("drop"),
        "the refusal must point at the drop: {refusal}"
    );
}

#[test]
fn a_foreign_key_to_a_dropped_table_is_refused() {
    let refusal = verdict(&format!(
        r#"{A},{B},{{"op":"dropTable","table":"b"}},{}"#,
        fk_to("b")
    ))
    .expect_err("the referenced table was dropped earlier");
    assert!(
        refusal.to_lowercase().contains("drop"),
        "the refusal must point at the drop: {refusal}"
    );
}

#[test]
fn a_foreign_key_to_a_renamed_away_table_is_refused() {
    verdict(&format!(
        r#"{A},{B},{{"op":"renameTable","table":"b","to":"b2"}},{}"#,
        fk_to("b")
    ))
    .expect_err("the referenced table no longer exists under that name");
}

#[test]
fn a_view_joining_a_dropped_table_is_refused() {
    // The join list is a second source of names beyond `from`.
    verdict(&format!(
        r#"{A},{B},{{"op":"dropTable","table":"b"}},{{"op":"createView","name":"vw","query":{{"kind":"structured","select":{{"from":{{"name":"a"}},"projection":[{{"kind":"colRef","name":"c0"}}],"joins":[{{"kind":"inner","table":{{"name":"b"}},"on":{{"node":"literal","value":true}}}}]}}}}}}"#
    ))
    .expect_err("a joined table that was dropped is just as absent as the from-table");
}

#[test]
fn detaching_a_partition_of_a_dropped_parent_is_refused() {
    verdict(&format!(
        r#"{PARENT},{P1},{{"op":"dropTable","table":"par"}},{{"op":"detachPartition","parent":"par","name":"p1"}}"#
    ))
    .expect_err("the parent of the detach was dropped");
}

#[test]
fn dropping_a_partition_of_a_dropped_parent_is_refused() {
    verdict(&format!(
        r#"{PARENT},{P1},{{"op":"dropTable","table":"par"}},{{"op":"dropPartition","parent":"par","name":"p1"}}"#
    ))
    .expect_err("the parent of the drop was dropped");
}

// ---------------------------------------------------------------------------
// Controls.
// ---------------------------------------------------------------------------

#[test]
fn a_forward_foreign_key_is_still_allowed() {
    // THE CONTROL THAT KEEPS THIS HONEST. A foreign key naming a table created
    // LATER in the same envelope is a real pattern - the engine defers the
    // constraint. Refusing every unknown name, rather than only vacated ones,
    // would break it.
    verdict(&format!(r#"{A},{},{B}"#, fk_to("b")))
        .expect("a foreign key to a table created later in the envelope must still pass");
}

#[test]
fn a_view_over_a_live_table_is_still_allowed() {
    verdict(&format!(r#"{A},{}"#, view_from("a"))).expect("the ordinary case must pass");
}

#[test]
fn a_foreign_key_to_a_live_table_is_still_allowed() {
    verdict(&format!(r#"{A},{B},{}"#, fk_to("b"))).expect("the ordinary case must pass");
}

#[test]
fn a_view_over_a_table_recreated_after_the_drop_is_allowed() {
    verdict(&format!(
        r#"{A},{{"op":"dropTable","table":"a"}},{A},{}"#,
        view_from("a")
    ))
    .expect("the restoring move this family has needed every time");
}

#[test]
fn a_foreign_key_to_the_targets_new_name_is_still_allowed() {
    verdict(&format!(
        r#"{A},{B},{{"op":"renameTable","table":"b","to":"b2"}},{}"#,
        fk_to("b2")
    ))
    .expect("naming the new name after a rename is the normal continuation");
}

#[test]
fn an_unrelated_drop_does_not_block_any_of_them() {
    verdict(&format!(
        r#"{A},{B},{{"op":"createTable","name":"other","columns":[{{"name":"c0","type":"int","nullable":false}}],"primaryKey":["c0"]}},{{"op":"dropTable","table":"other"}},{},{}"#,
        fk_to("b"),
        view_from("a")
    ))
    .expect("dropping an unrelated table must not block a view or a foreign key");
}
