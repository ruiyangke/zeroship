//! Tables, views and sequences share ONE relation namespace, and the engine now
//! treats them that way.
//!
//! `name_claimed_twice_in_one_migration.rs` closed the table-vs-table case. It
//! tracked TABLES, so a view or a sequence could still take a live name. Measured
//! and confirmed against live PostgreSQL - each of these lowered:
//!
//!     CREATE VIEW "public"."vw" ...  x2          ERROR: relation "vw" already exists
//!     CREATE SEQUENCE "public"."sq"  x2          ERROR: relation "sq" already exists
//!     CREATE VIEW "vw"; CREATE TABLE "vw" (...)  ERROR: relation "vw" already exists
//!     CREATE SEQUENCE "sq"; CREATE TABLE "sq"    ERROR: relation "sq" already exists
//!
//! THE ASYMMETRY THIS CLOSES is the same one the sibling fixtures record, and by
//! this point it is a pattern rather than a coincidence. Redefining an object
//! twice in one migration was ALREADY refused for three kinds:
//!
//!   - `createEnum` twice    -> `duplicate definition` at lower
//!   - `createDomain` twice  -> `duplicate definition` at lower
//!   - `createIndex` twice   -> refused at the gate, with a precise message about
//!     `IF NOT EXISTS` silently skipping the second
//!
//! Views and sequences were the two kinds nobody had gotten to.
//!
//! WHY A SHARED NAMESPACE RATHER THAN THREE SETS: PostgreSQL keeps tables, views
//! and sequences in one relation namespace, so the cross-kind collisions above are
//! errors even though no single kind repeats. Three per-kind sets would catch the
//! first two lines and miss the last two.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn verdict(ops: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, Dialect::Postgres, &[]).map_err(|e| format!("{}: {}", e.code, e.reason))
}

const A: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false}],"primaryKey":["c0"]}"#;
const VIEW: &str = r#"{"op":"createView","name":"vw","query":{"kind":"structured","select":{"from":{"name":"a"},"projection":[{"kind":"colRef","name":"c0"}]}}}"#;
const SEQ: &str = r#"{"op":"createSequence","name":"sq"}"#;

fn table_named(name: &str) -> String {
    format!(
        r#"{{"op":"createTable","name":"{name}","columns":[{{"name":"c0","type":"int","nullable":false}}],"primaryKey":["c0"]}}"#
    )
}

#[test]
fn creating_the_same_view_twice_is_refused() {
    let refusal =
        verdict(&format!("{A},{VIEW},{VIEW}")).expect_err("the second createView retakes a name");
    assert!(
        refusal.to_lowercase().contains("already"),
        "the refusal must say the name is already taken: {refusal}"
    );
}

#[test]
fn creating_the_same_sequence_twice_is_refused() {
    verdict(&format!("{SEQ},{SEQ}")).expect_err("the second createSequence retakes a name");
}

#[test]
fn a_table_may_not_take_a_live_view_name() {
    verdict(&format!("{A},{VIEW},{}", table_named("vw")))
        .expect_err("a table and a view share the relation namespace");
}

#[test]
fn a_table_may_not_take_a_live_sequence_name() {
    verdict(&format!("{SEQ},{}", table_named("sq")))
        .expect_err("a table and a sequence share the relation namespace");
}

#[test]
fn a_view_may_not_take_a_live_table_name() {
    // The reverse direction. A rule written only into the createTable arm would
    // pass the three above and miss this.
    verdict(&format!(
        r#"{A},{{"op":"createView","name":"a","query":{{"kind":"structured","select":{{"from":{{"name":"a"}},"projection":[{{"kind":"colRef","name":"c0"}}]}}}}}}"#
    ))
    .expect_err("a view may not take the name of a table this envelope created");
}

// ---------------------------------------------------------------------------
// Controls.
// ---------------------------------------------------------------------------

#[test]
fn dropping_a_view_then_recreating_it_is_still_allowed() {
    verdict(&format!(
        r#"{A},{VIEW},{{"op":"dropView","name":"vw"}},{VIEW}"#
    ))
    .expect("drop then recreate is how a view's query is replaced");
}

#[test]
fn dropping_a_sequence_then_recreating_it_is_still_allowed() {
    verdict(&format!(
        r#"{SEQ},{{"op":"dropSequence","name":"sq"}},{SEQ}"#
    ))
    .expect("drop then recreate must remain allowed for sequences too");
}

#[test]
fn a_table_may_take_a_view_name_that_was_dropped_first() {
    verdict(&format!(
        r#"{A},{VIEW},{{"op":"dropView","name":"vw"}},{}"#,
        table_named("vw")
    ))
    .expect("the relation name is free once the view is dropped");
}

#[test]
fn distinct_names_across_the_three_kinds_are_still_allowed() {
    verdict(&format!("{A},{VIEW},{SEQ}"))
        .expect("a table, a view and a sequence with distinct names are ordinary");
}

#[test]
fn a_view_and_a_sequence_do_not_disturb_column_tracking() {
    // The relation set and the per-table column map are separate things; adding
    // views and sequences to the first must not perturb the second.
    verdict(&format!(
        r#"{A},{VIEW},{SEQ},{{"op":"addColumn","table":"a","column":"n","type":"int","nullable":true}}"#
    ))
    .expect("ordinary column work alongside a view and a sequence must still pass");
}
