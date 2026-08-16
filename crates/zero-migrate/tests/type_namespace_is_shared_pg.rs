//! Enums, domains, tables and views share PostgreSQL's TYPE namespace.
//!
//! The type-side analogue of `relation_namespace_is_shared.rs`. `createEnum`
//! twice and `createDomain` twice were each already refused at lower with
//! `duplicate definition`, but nothing checked one KIND against the other, and
//! nothing knew that every table and view also creates a composite row type.
//!
//! MEASURED AGAINST LIVE POSTGRESQL, every cell:
//!
//!     CREATE TYPE e AS ENUM; CREATE DOMAIN e     type "e" already exists
//!     CREATE DOMAIN d;       CREATE TYPE d       type "d" already exists
//!     CREATE TABLE t;        CREATE TYPE t       type "t" already exists
//!     CREATE TYPE e;         CREATE TABLE e      type "e" already exists
//!     CREATE DOMAIN dm;      CREATE TABLE dm     type "dm" already exists
//!     CREATE TYPE vv;        CREATE VIEW vv      type "vv" already exists
//!     CREATE TYPE n;         CREATE SEQUENCE n   type "n" already exists
//!
//!     CREATE SEQUENCE n;     CREATE TYPE n       ACCEPTED
//!
//! THE LAST LINE IS WHY THIS IS NOT A NAMESPACE MERGE. Every other pair collides
//! in both directions, so the obvious fix - fold types into the relation
//! namespace - passes all seven refusal cases and REFUSES that last one, which
//! PostgreSQL accepts. It was surprising enough that it was re-measured in
//! isolation before being believed.
//!
//! The model the fix implements, stated as claim-vs-check:
//!
//!     table     CLAIMS relation, CLAIMS type   (its composite row type)
//!     view      CLAIMS relation, CLAIMS type
//!     sequence  CLAIMS relation, CHECKS type but does not hold it
//!     enum      CLAIMS type
//!     domain    CLAIMS type
//!
//! POSTGRESQL ONLY. Enums and domains are emulated rather than declared on the
//! other two dialects, so there is no second namespace there to collide with.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn verdict(ops: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, Dialect::Postgres, &[]).map_err(|e| format!("{}: {}", e.code, e.reason))
}

fn tbl(n: &str) -> String {
    format!(
        r#"{{"op":"createTable","name":"{n}","columns":[{{"name":"c0","type":"int","nullable":false}}],"primaryKey":["c0"]}}"#
    )
}
fn enm(n: &str) -> String {
    format!(r#"{{"op":"createEnum","name":"{n}","values":["a"]}}"#)
}
fn dom(n: &str) -> String {
    format!(r#"{{"op":"createDomain","name":"{n}","as":"text"}}"#)
}

#[test]
fn an_enum_and_a_domain_may_not_share_a_name() {
    let refusal = verdict(&format!("{},{}", enm("e"), dom("e")))
        .expect_err("enums and domains are both types");
    assert!(
        refusal.to_lowercase().contains("already"),
        "the refusal must say the name is already taken: {refusal}"
    );
}

#[test]
fn a_domain_and_an_enum_may_not_share_a_name() {
    verdict(&format!("{},{}", dom("d"), enm("d"))).expect_err("the reverse direction");
}

#[test]
fn an_enum_may_not_take_a_live_table_name() {
    // Every table creates a composite row type of the same name.
    verdict(&format!("{},{}", tbl("t"), enm("t")))
        .expect_err("a table's composite row type occupies the type namespace");
}

#[test]
fn a_table_may_not_take_a_live_enum_name() {
    verdict(&format!("{},{}", enm("t"), tbl("t"))).expect_err("the reverse direction");
}

#[test]
fn a_table_may_not_take_a_live_domain_name() {
    verdict(&format!("{},{}", dom("dm"), tbl("dm"))).expect_err("domains are types too");
}

#[test]
fn a_view_may_not_take_a_live_enum_name() {
    verdict(&format!(
        r#"{},{},{{"op":"createView","name":"vv","query":{{"kind":"structured","select":{{"from":{{"name":"a"}},"projection":[{{"kind":"colRef","name":"c0"}}]}}}}}}"#,
        tbl("a"),
        enm("vv")
    ))
    .expect_err("a view creates a composite row type as well");
}

#[test]
fn a_sequence_may_not_take_a_live_enum_name() {
    verdict(&format!(
        r#"{},{{"op":"createSequence","name":"n"}}"#,
        enm("n")
    ))
    .expect_err("creating a sequence requires the type name to be free");
}

// ---------------------------------------------------------------------------
// Controls. The first is the one that decides the SHAPE of the fix.
// ---------------------------------------------------------------------------

#[test]
fn an_enum_may_take_a_live_sequence_name() {
    // MEASURED AND RE-MEASURED IN ISOLATION: PostgreSQL accepts this, even though
    // the reverse collides. A merged namespace - the obvious implementation -
    // would refuse it. This control is the whole reason the fix distinguishes
    // CLAIMING a namespace from merely CHECKING it.
    verdict(&format!(
        r#"{{"op":"createSequence","name":"n"}},{}"#,
        enm("n")
    ))
    .expect("PostgreSQL accepts an enum named after a live sequence");
}

#[test]
fn dropping_an_enum_frees_the_name_for_a_table() {
    verdict(&format!(
        r#"{},{{"op":"dropEnum","name":"x"}},{}"#,
        enm("x"),
        tbl("x")
    ))
    .expect("the type name is free once the enum is dropped");
}

#[test]
fn dropping_a_table_frees_the_name_for_an_enum() {
    verdict(&format!(
        r#"{},{{"op":"dropTable","table":"x"}},{}"#,
        tbl("x"),
        enm("x")
    ))
    .expect("the composite row type goes with the table");
}

#[test]
fn distinct_names_across_all_the_kinds_are_still_allowed() {
    verdict(&format!(
        r#"{},{},{},{{"op":"createSequence","name":"sq"}}"#,
        tbl("t"),
        enm("e"),
        dom("d")
    ))
    .expect("a table, an enum, a domain and a sequence with distinct names are ordinary");
}
