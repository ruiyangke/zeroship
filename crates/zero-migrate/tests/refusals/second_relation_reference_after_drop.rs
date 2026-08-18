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

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn verdict(ops: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, Dialect::Postgres).map_err(|e| format!("{}: {}", e.code, e.reason))
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

/// Assert the refusal names the ROLE this test is about, not merely that one
/// happened.
///
/// Added by the F769 audit. The previous assertions here checked only that the
/// message contained "drop", which every member of this family satisfies - a
/// foreign-key test would have passed on a view-source refusal and vice versa.
/// The role word is what distinguishes them, and it is the thing each test is
/// actually claiming.
fn expect_reference_refusal(ops: &str, role: &str, what: &str) {
    let refusal = verdict(ops).expect_err(what);
    let expected = format!("names {role}");
    assert!(
        refusal.contains(&expected),
        "the refusal must name the {role:?} this test is about, not another \
         reference in the same family: {refusal}"
    );
}

#[test]
fn a_view_reading_a_dropped_table_is_refused() {
    expect_reference_refusal(
        &format!(r#"{A},{{"op":"dropTable","table":"a"}},{}"#, view_from("a")),
        "source table",
        "the view's source table was dropped earlier",
    );
}

#[test]
fn a_foreign_key_to_a_dropped_table_is_refused() {
    expect_reference_refusal(
        &format!(r#"{A},{B},{{"op":"dropTable","table":"b"}},{}"#, fk_to("b")),
        "referenced table",
        "the referenced table was dropped earlier",
    );
}

#[test]
fn a_foreign_key_to_a_renamed_away_table_is_refused() {
    expect_reference_refusal(
        &format!(
            r#"{A},{B},{{"op":"renameTable","table":"b","to":"b2"}},{}"#,
            fk_to("b")
        ),
        "referenced table",
        "the referenced table no longer exists under that name",
    );
}

#[test]
fn a_view_joining_a_dropped_table_is_refused() {
    // The join list is a second source of names beyond `from`, and the role word
    // is what proves the JOIN was read rather than the from-table.
    expect_reference_refusal(
        &format!(
            r#"{A},{B},{{"op":"dropTable","table":"b"}},{{"op":"createView","name":"vw","query":{{"kind":"structured","select":{{"from":{{"name":"a"}},"projection":[{{"kind":"colRef","name":"c0"}}],"joins":[{{"kind":"inner","table":{{"name":"b"}},"on":{{"node":"literal","value":true}}}}]}}}}}}"#
        ),
        "joined table",
        "a joined table that was dropped is just as absent as the from-table",
    );
}

#[test]
fn detaching_a_partition_of_a_dropped_parent_is_refused() {
    expect_reference_refusal(
        &format!(
            r#"{PARENT},{P1},{{"op":"dropTable","table":"par"}},{{"op":"detachPartition","parent":"par","name":"p1"}}"#
        ),
        "parent table",
        "the parent of the detach was dropped",
    );
}

#[test]
fn dropping_a_partition_of_a_dropped_parent_is_refused() {
    expect_reference_refusal(
        &format!(
            r#"{PARENT},{P1},{{"op":"dropTable","table":"par"}},{{"op":"dropPartition","parent":"par","name":"p1"}}"#
        ),
        "parent table",
        "the parent of the drop was dropped",
    );
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

// ---------------------------------------------------------------------------
// WHY THIS CHECK IS DIALECT-UNIFORM, measured rather than defaulted.
// ---------------------------------------------------------------------------

/// The sibling fixtures in this session establish a house rule: DO NOT REFUSE
/// WHAT THE DATABASE ACCEPTS. `duplicate_constraint_name_on_one_table.rs` exempts
/// SQLite on those grounds, and `index_shares_the_relation_namespace.rs` exempts
/// MySQL.
///
/// Applied mechanically, that rule says this check should exempt SQLite too:
///
///     CREATE TABLE s (c0 int); DROP TABLE s;
///     CREATE VIEW v AS SELECT c0 FROM s;      -- SQLite ACCEPTS this
///
/// MEASURED FURTHER, and this is what settles it. SQLite accepts the statement
/// and the view appears in `sqlite_master` - but querying it fails:
///
///     SELECT * FROM v   ->   no such table: main.s
///
/// So the acceptance is DEFERRED FAILURE, not success. The migration reports
/// success and leaves a permanently broken view that fails at first read, which
/// is strictly worse for an operator than being told at authoring time.
///
/// THE HOUSE RULE THEREFORE NEEDS ITS QUALIFIER STATED: do not refuse what the
/// database accepts AND THEN HONOURS. Where a dialect accepts a statement by
/// postponing the error to read time, matching it would be matching the letter of
/// the behaviour against the point of the check.
///
/// This test exists so the uniformity is a RECORDED DECISION rather than an
/// unexamined default - a future reader applying the house rule mechanically
/// would otherwise exempt SQLite here and reintroduce broken views.
#[test]
fn sqlite_is_refused_too_because_its_acceptance_is_only_deferred_failure() {
    let bytes = format!(
        r#"{{"ir_version":1,"name":"n","ops":[{A},{{"op":"dropTable","table":"a"}},{}]}}"#,
        view_from("a")
    );
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, Dialect::Sqlite)
        .expect_err("SQLite accepts this DDL but the view it creates can never be read");
}
