//! A dropped enum, domain or sequence may not be named by `addColumn`,
//! `setColumnType` or `alterSequence` either.
//!
//! `validate_no_column_uses_a_dropped_named_object` already refused a
//! `createTable` whose column named a type this migration dropped. It collected
//! `IrColumn`s, and `createTable` is the only op that HAS them: `addColumn` and
//! `setColumnType` carry their type as loose fields, so an op with a type but no
//! column was invisible to the walk. Both emit that type into the SQL.
//!
//! MEASURED AGAINST LIVE POSTGRESQL:
//!
//!     CREATE TYPE e AS ENUM (...); DROP TYPE e;
//!     ALTER TABLE t ADD COLUMN v e           ERROR: type "e" does not exist
//!     CREATE DOMAIN d AS int; DROP DOMAIN d;
//!     ALTER TABLE t ADD COLUMN v d           ERROR: type "d" does not exist
//!     CREATE SEQUENCE sq; DROP SEQUENCE sq;
//!     ALTER SEQUENCE sq INCREMENT BY 2       ERROR: relation "sq" does not exist
//!
//! `renameColumn` ALSO CARRIES A TYPE AND IS DELIBERATELY NOT CHECKED. It is
//! metadata describing the column after the rename; `ALTER TABLE … RENAME COLUMN`
//! never mentions a type, so nothing there can fail to resolve. Adding it because
//! the field is present - the obvious move when sweeping for "ops that carry a
//! ColType" - would refuse a migration the server runs. A control pins it.
//!
//! STILL UNCHECKED, MEASURED AND RECORDED RATHER THAN LEFT IMPLIED: two more
//! use-after-drop shapes in this family have no rule yet, because they live in
//! namespaces this walk does not track at all.
//!
//!     CREATE ROLE r; DROP ROLE r; GRANT SELECT ON t TO r
//!         ERROR: role "r" does not exist
//!     CREATE SCHEMA s; DROP SCHEMA s; CREATE TABLE s.x (...)
//!         ERROR: schema "s" does not exist
//!
//! Both are accepted by the engine today. They need a role map and a schema map,
//! not another entry in this one, so they are a separate piece of work rather
//! than something quietly bundled here.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn verdict(ops: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, Dialect::Postgres, &[]).map_err(|e| format!("{}: {}", e.code, e.reason))
}

fn expect_dependency_refusal(ops: &str, what: &str) {
    let refusal = verdict(ops).expect_err(what);
    assert!(
        !refusal.contains("VENDOR_OP_DENIED"),
        "this must be refused for the dropped dependency, not by the capability \
         gate: {refusal}"
    );
    assert!(
        refusal.contains("will not exist when this runs"),
        "the refusal must be the use-after-drop one: {refusal}"
    );
}

const T: &str = r#"{"op":"createTable","name":"t","columns":[{"name":"c0","type":"int","nullable":false}],"primaryKey":["c0"]}"#;
const MAKE_ENUM: &str = r#"{"op":"createEnum","name":"e","values":["a","b"]}"#;
const DROP_ENUM: &str = r#"{"op":"dropEnum","name":"e"}"#;
const MAKE_DOMAIN: &str = r#"{"op":"createDomain","name":"d","as":"int"}"#;
const DROP_DOMAIN: &str = r#"{"op":"dropDomain","name":"d"}"#;
const MAKE_SEQ: &str = r#"{"op":"createSequence","name":"sq"}"#;
const DROP_SEQ: &str = r#"{"op":"dropSequence","name":"sq"}"#;
const ADD_ENUM_COL: &str =
    r#"{"op":"addColumn","table":"t","column":"v","type":{"enum":{"name":"e"}},"nullable":true}"#;
const ADD_DOMAIN_COL: &str =
    r#"{"op":"addColumn","table":"t","column":"v","type":{"domain":{"name":"d"}},"nullable":true}"#;
const ADD_NEXTVAL_COL: &str = r#"{"op":"addColumn","table":"t","column":"v","type":"int","nullable":true,"default":{"nextval":{"name":"sq"}}}"#;
const SET_ENUM_TYPE: &str =
    r#"{"op":"setColumnType","table":"t","column":"c0","toType":{"enum":{"name":"e"}}}"#;
const ALTER_SEQ: &str = r#"{"op":"alterSequence","name":"sq","increment":2}"#;

// ---------------------------------------------------------------------------
// Refusals.
// ---------------------------------------------------------------------------

#[test]
fn add_column_typed_with_a_dropped_enum_is_refused() {
    expect_dependency_refusal(
        &format!("{MAKE_ENUM},{T},{DROP_ENUM},{ADD_ENUM_COL}"),
        "the enum is gone, so the added column cannot resolve its type",
    );
}

#[test]
fn add_column_typed_with_a_dropped_domain_is_refused() {
    expect_dependency_refusal(
        &format!("{MAKE_DOMAIN},{T},{DROP_DOMAIN},{ADD_DOMAIN_COL}"),
        "the domain is gone, so the added column cannot resolve its type",
    );
}

#[test]
fn add_column_defaulting_from_a_dropped_sequence_is_refused() {
    // The default is the other half of the dependency pair, and an
    // implementation that only read the TYPE would pass the two tests above and
    // fail this one.
    expect_dependency_refusal(
        &format!("{MAKE_SEQ},{T},{DROP_SEQ},{ADD_NEXTVAL_COL}"),
        "the sequence is gone, so the nextval default cannot resolve",
    );
}

#[test]
fn set_column_type_to_a_dropped_enum_is_refused() {
    expect_dependency_refusal(
        &format!("{MAKE_ENUM},{T},{DROP_ENUM},{SET_ENUM_TYPE}"),
        "the enum is gone, so the new column type cannot resolve",
    );
}

#[test]
fn altering_a_dropped_sequence_is_refused() {
    expect_dependency_refusal(
        &format!("{MAKE_SEQ},{DROP_SEQ},{ALTER_SEQ}"),
        "the sequence is gone, so the alter cannot resolve it",
    );
}

// ---------------------------------------------------------------------------
// Controls.
// ---------------------------------------------------------------------------

#[test]
fn add_column_typed_with_a_live_enum_is_still_allowed() {
    verdict(&format!("{MAKE_ENUM},{T},{ADD_ENUM_COL}"))
        .expect("adding an enum-typed column is the ordinary case");
}

#[test]
fn recreating_the_enum_before_use_is_still_allowed() {
    // Dropping an enum and recreating it is how its value set is replaced, so
    // the name must come back live.
    verdict(&format!(
        "{MAKE_ENUM},{T},{DROP_ENUM},{MAKE_ENUM},{ADD_ENUM_COL}"
    ))
    .expect("the recreated enum resolves");
}

#[test]
fn recreating_the_sequence_before_altering_it_is_still_allowed() {
    verdict(&format!("{MAKE_SEQ},{DROP_SEQ},{MAKE_SEQ},{ALTER_SEQ}"))
        .expect("the recreated sequence resolves");
}

#[test]
fn rename_column_carrying_a_dropped_enum_type_is_still_allowed() {
    // THE DELIBERATE EXCLUSION. `renameColumn` carries a type, but the rendered
    // `ALTER TABLE … RENAME COLUMN` never mentions one, so a dropped enum there
    // cannot fail to resolve. Sweeping every op that carries a ColType into the
    // check - the obvious implementation - refuses this.
    //
    // The table is NOT created here on purpose. A separate pre-existing rule
    // requires a renameColumn to be the only operation targeting its table, so
    // an envelope that also created the table refused for that reason instead -
    // a control that fails for the wrong reason measures nothing.
    verdict(&format!(
        r#"{MAKE_ENUM},{DROP_ENUM},{{"op":"renameColumn","table":"t","from":"v","to":"w","type":{{"enum":{{"name":"e"}}}}}}"#
    ))
    .expect("a rename does not re-emit the column type");
}

#[test]
fn altering_a_live_sequence_is_still_allowed() {
    verdict(&format!("{MAKE_SEQ},{ALTER_SEQ}")).expect("altering a live sequence is ordinary");
}

#[test]
fn an_unrelated_drop_does_not_disturb_an_ordinary_add_column() {
    verdict(&format!(
        r#"{MAKE_ENUM},{T},{DROP_ENUM},{{"op":"addColumn","table":"t","column":"v","type":"int","nullable":true}}"#
    ))
    .expect("a plain int column is unaffected by a dropped enum");
}

#[test]
fn the_create_table_case_is_still_refused() {
    // The behaviour that already worked, kept under test so the refactor that
    // generalised the walk cannot quietly drop it.
    expect_dependency_refusal(
        &format!(
            r#"{MAKE_ENUM},{DROP_ENUM},{{"op":"createTable","name":"t2","columns":[{{"name":"c0","type":"int","nullable":false}},{{"name":"v","type":{{"enum":{{"name":"e"}}}},"nullable":true}}],"primaryKey":["c0"]}}"#
        ),
        "the original createTable case must keep failing",
    );
}
