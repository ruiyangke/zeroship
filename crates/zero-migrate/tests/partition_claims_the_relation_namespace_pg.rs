//! A partition is a relation: it claims both namespaces, like any table.
//!
//! The last name-claiming op the model missed. F714-F717 taught the engine that
//! tables, views, sequences and indexes share PostgreSQL's relation namespace and
//! that tables and views also occupy the type namespace via their composite row
//! type. `createPartition` does all of that - `CREATE TABLE ... PARTITION OF` is
//! a CREATE TABLE - and was tracked by none of it.
//!
//! MEASURED AGAINST LIVE POSTGRESQL:
//!
//!     CREATE TABLE p1 PARTITION OF a ...  x2      relation "p1" already exists
//!     CREATE TABLE b (...);
//!     CREATE TABLE b PARTITION OF a ...           relation "b" already exists
//!     CREATE TYPE e AS ENUM ('a');
//!     CREATE TABLE e PARTITION OF a ...           type "e" already exists
//!
//! That third line is the one that says a partition takes the TYPE namespace too,
//! not just the relation namespace - it has a composite row type exactly as an
//! ordinary table does.
//!
//! DETACH DOES NOT FREE THE NAME, and that is the control that shapes the fix. A
//! detached partition becomes a standalone TABLE under the same name, so the name
//! stays occupied. Only `dropPartition` releases it. A fix that treated
//! `detachPartition` as a release would wrongly accept a later create.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn verdict(ops: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, Dialect::Postgres, &[]).map_err(|e| format!("{}: {}", e.code, e.reason))
}

const PARENT: &str = r#"{"op":"createTable","name":"par","columns":[{"name":"c0","type":"int","nullable":false}],"primaryKey":["c0"],"partitionBy":{"kind":"range","columns":["c0"]}}"#;

fn part(name: &str, from: i32, to: i32) -> String {
    format!(
        r#"{{"op":"createPartition","name":"{name}","of":"par","bounds":{{"kind":"range","from":[{{"kind":"int","value":{from}}}],"to":[{{"kind":"int","value":{to}}}]}}}}"#
    )
}
fn tbl(n: &str) -> String {
    format!(
        r#"{{"op":"createTable","name":"{n}","columns":[{{"name":"c0","type":"int","nullable":false}}],"primaryKey":["c0"]}}"#
    )
}

#[test]
fn the_same_partition_name_twice_is_refused() {
    let refusal = verdict(&format!(
        "{PARENT},{},{}",
        part("p1", 0, 10),
        part("p1", 10, 20)
    ))
    .expect_err("the second createPartition retakes a relation name");
    assert!(
        refusal.to_lowercase().contains("already"),
        "the refusal must say the name is already taken: {refusal}"
    );
}

#[test]
fn a_partition_may_not_take_a_live_table_name() {
    verdict(&format!("{PARENT},{},{}", tbl("b"), part("b", 0, 10)))
        .expect_err("a partition and a table share the relation namespace");
}

#[test]
fn a_table_may_not_take_a_live_partition_name() {
    // The reverse direction, which a fix written only into the createPartition
    // arm would miss.
    verdict(&format!("{PARENT},{},{}", part("p1", 0, 10), tbl("p1")))
        .expect_err("a partition occupies the name against a later table");
}

#[test]
fn a_partition_may_not_take_a_live_enum_name() {
    verdict(&format!(
        r#"{PARENT},{{"op":"createEnum","name":"e","values":["a"]}},{}"#,
        part("e", 0, 10)
    ))
    .expect_err("a partition has a composite row type, so it needs the type name free");
}

#[test]
fn an_enum_may_not_take_a_live_partition_name() {
    verdict(&format!(
        r#"{PARENT},{},{{"op":"createEnum","name":"p1","values":["a"]}}"#,
        part("p1", 0, 10)
    ))
    .expect_err("the partition's row type occupies the type namespace");
}

// ---------------------------------------------------------------------------
// Controls.
// ---------------------------------------------------------------------------

#[test]
fn distinct_partition_names_are_still_allowed() {
    verdict(&format!(
        "{PARENT},{},{}",
        part("p1", 0, 10),
        part("p2", 10, 20)
    ))
    .expect("two partitions of one parent with distinct names are the ordinary case");
}

#[test]
fn dropping_a_partition_frees_its_name() {
    verdict(&format!(
        r#"{PARENT},{},{{"op":"dropPartition","parent":"par","name":"p1"}},{}"#,
        part("p1", 0, 10),
        part("p1", 10, 20)
    ))
    .expect("dropping a partition releases its relation name");
}

#[test]
fn detaching_a_partition_does_not_free_its_name() {
    // THE CONTROL THAT SHAPES THE FIX. A detached partition becomes a standalone
    // TABLE under the same name, so the name is still taken. Treating detach as a
    // release would wrongly accept this.
    verdict(&format!(
        r#"{PARENT},{},{{"op":"detachPartition","parent":"par","name":"p1"}},{}"#,
        part("p1", 0, 10),
        tbl("p1")
    ))
    .expect_err("a detached partition still occupies its name as a standalone table");
}

#[test]
fn a_partition_after_dropping_the_colliding_table_is_allowed() {
    verdict(&format!(
        r#"{PARENT},{},{{"op":"dropTable","table":"b"}},{}"#,
        tbl("b"),
        part("b", 0, 10)
    ))
    .expect("the name is free once the table is dropped");
}

// ---------------------------------------------------------------------------
// Dropping the PARENT releases its partitions' names too.
// ---------------------------------------------------------------------------

/// A FALSE REFUSAL this fixture's own rule introduced, found by probing what
/// happens when two of the session's rules apply to one envelope.
///
/// A partition is a dependent object: dropping the partitioned parent drops its
/// partitions with it. Measured against live PostgreSQL:
///
///     CREATE TABLE par (...) PARTITION BY RANGE (c0);
///     CREATE TABLE p1 PARTITION OF par FOR VALUES FROM (0) TO (10);
///     DROP TABLE par;
///     -- information_schema now reports 0 tables named p1
///     CREATE TABLE p1 (c0 int);      -- SUCCEEDS
///
/// The relation map released `par` on the drop but kept `p1`, so a later use of
/// that freed name was refused. The engine was stricter than the database, which
/// is the failure mode `new_rules_do_not_over_refuse.rs` exists to prevent and
/// which no single-rule test could surface: it takes a drop AND a later claim in
/// the same envelope.
#[test]
fn dropping_the_parent_frees_its_partitions_names() {
    verdict(&format!(
        r#"{PARENT},{},{{"op":"dropTable","table":"par"}},{}"#,
        part("p1", 0, 10),
        tbl("p1")
    ))
    .expect("dropping the parent drops p1 with it, so the name is free again");
}

#[test]
fn a_recreated_parent_may_take_the_same_partition_names() {
    // The whole point of the shape: tear the partitioned table down and rebuild
    // it with the same partition layout.
    verdict(&format!(
        r#"{PARENT},{},{{"op":"dropTable","table":"par"}},{PARENT},{}"#,
        part("p1", 0, 10),
        part("p1", 0, 10)
    ))
    .expect("rebuilding a partitioned table with the same partition names is ordinary");
}

#[test]
fn dropping_an_unrelated_table_does_not_free_a_partition_name() {
    // THE CONTROL. Releasing every partition on any drop would pass the two tests
    // above and lose the protection F722 added.
    verdict(&format!(
        r#"{PARENT},{},{},{{"op":"dropTable","table":"other"}},{}"#,
        tbl("other"),
        part("p1", 0, 10),
        part("p1", 10, 20)
    ))
    .expect_err("an unrelated drop must not release a live partition name");
}
