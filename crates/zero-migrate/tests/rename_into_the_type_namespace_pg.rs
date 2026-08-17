//! A `renameTable` must find its target name free in the TYPE namespace too.
//!
//! The gap F716's own model implies but its fix did not cover. F716 taught the
//! engine that a table carries a composite row type; renaming a table renames
//! that type with it, so the target name has to be free on BOTH sides. The
//! renameTable arm consulted only the relation namespace.
//!
//! MEASURED AGAINST LIVE POSTGRESQL:
//!
//!     CREATE TYPE e AS ENUM ('a');
//!     ALTER TABLE a RENAME TO e;        ERROR: type "e" already exists
//!
//!     CREATE DOMAIN dm AS text;
//!     ALTER TABLE b RENAME TO dm;       ERROR: type "dm" already exists
//!
//! AND THE CELL THAT KEEPS THIS NARROW, measured in the same pass:
//!
//!     CREATE TYPE e AS ENUM ('a');
//!     CREATE INDEX e ON a (v);          ACCEPTED
//!
//! An index has no row type, so it does not consult the type namespace at all.
//! Widening "relation creation checks the type namespace" to indexes would refuse
//! that, and it is only visible if you measure the kinds separately rather than
//! reasoning from "indexes are relations" - which F715 established and which is
//! true, but does not settle this.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn verdict(ops: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, Dialect::Postgres).map_err(|e| format!("{}: {}", e.code, e.reason))
}

/// Assert the claiming op and the kind holding the name.
///
/// The two renameTable tests below differ only in whether an ENUM or a DOMAIN
/// holds the target name; a guard on the rule alone lets each pass on the
/// other message.
fn expect_type_refusal(ops: &str, claiming_op: &str, held_by: &str, what: &str) {
    let refusal = verdict(ops).expect_err(what);
    for needle in [
        "one type namespace",
        &format!("this {claiming_op} claims"),
        &format!("already created a {held_by} with that name"),
    ] {
        assert!(
            refusal.contains(needle),
            "{needle:?} is missing, so a sibling rule satisfies this test: {refusal}"
        );
    }
}

const A: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true}],"primaryKey":["c0"]}"#;

#[test]
fn renaming_a_table_onto_a_live_enum_name_is_refused() {
    expect_type_refusal(
        &format!(
            r#"{A},{{"op":"createEnum","name":"e","values":["a"]}},{{"op":"renameTable","table":"a","to":"e"}}"#
        ),
        "renameTable",
        "enum",
        "the rename target is taken in the type namespace",
    );
}

#[test]
fn renaming_a_table_onto_a_live_domain_name_is_refused() {
    expect_type_refusal(
        &format!(
            r#"{A},{{"op":"createDomain","name":"dm","as":"text"}},{{"op":"renameTable","table":"a","to":"dm"}}"#
        ),
        "renameTable",
        "domain",
        "a domain occupies the type namespace just as an enum does",
    );
}

// ---------------------------------------------------------------------------
// Controls.
// ---------------------------------------------------------------------------

#[test]
fn an_index_may_still_take_a_live_enum_name() {
    // MEASURED: PostgreSQL accepts this. An index has no row type. The tempting
    // generalisation - "relations check the type namespace, and F715 showed
    // indexes are relations" - would refuse a migration the server runs.
    verdict(&format!(
        r#"{A},{{"op":"createEnum","name":"e","values":["a"]}},{{"op":"createIndex","name":"e","table":"a","columns":[{{"kind":"column","name":"v"}}]}}"#
    ))
    .expect("PostgreSQL accepts an index named after a live enum");
}

#[test]
fn renaming_onto_a_type_name_that_was_dropped_first_is_allowed() {
    verdict(&format!(
        r#"{A},{{"op":"createEnum","name":"e","values":["a"]}},{{"op":"dropEnum","name":"e"}},{{"op":"renameTable","table":"a","to":"e"}}"#
    ))
    .expect("the name is free once the enum is dropped");
}

#[test]
fn an_ordinary_rename_is_still_allowed() {
    verdict(&format!(
        r#"{A},{{"op":"renameTable","table":"a","to":"b"}}"#
    ))
    .expect("a rename to a free name must pass");
}

#[test]
fn a_renamed_table_carries_its_row_type_to_the_new_name() {
    // After the rename the OLD name is free in both namespaces and the NEW one is
    // taken in both. Pins that the type entry moves rather than being dropped.
    expect_type_refusal(
        &format!(
            r#"{A},{{"op":"renameTable","table":"a","to":"b"}},{{"op":"createEnum","name":"b","values":["a"]}}"#
        ),
        "createEnum",
        "table",
        "an enum may not take the name the table was renamed onto",
    );

    verdict(&format!(
        r#"{A},{{"op":"renameTable","table":"a","to":"b"}},{{"op":"createEnum","name":"a","values":["a"]}}"#
    ))
    .expect("the vacated name is free for an enum");
}
