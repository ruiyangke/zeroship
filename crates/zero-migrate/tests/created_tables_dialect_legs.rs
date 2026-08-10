//! **A table created inside a dialect leg is registered as owned.**
//!
//! `LoweredArtifact.created_tables` is what the deploy loop folds into the cross-file
//! ownership registry - `effective_registry.entry(table).or_insert_with(|| app)` in
//! `engine.rs` - so a name missing from it is a name NO app claims. The next app to
//! declare that table then takes it without conflict.
//!
//! The list was built with `op_created_table`, whose `_ => None` arm swallows
//! `Op::Dialectal`, so a create authored inside `dialect({ ... })` was applied to the
//! database and left unowned. The SQL lowering in the same function descends
//! correctly, which is what makes this shape easy to miss: the table appears, the
//! claim does not.
//!
//! ALL LEGS, not the selected one, and that is a deliberate choice rather than a reuse
//! of the fold's rule:
//!
//!   - It is the FAIL-CLOSED direction. Over-claiming a name costs a later refusal
//!     that names the owner; under-claiming lets another app take a table this one
//!     authored. Only one of those is recoverable by reading an error message.
//!   - It matches the LOAD gate. `enforce_ir_ownership` already walks every leg when
//!     it decides who owns what, so a selected-leg registry would demand ownership the
//!     bookkeeping never records - two mechanisms disagreeing about one fact.
//!   - Ownership is not a property of the target. The same authored file deploys to
//!     PostgreSQL and to SQLite, and the app that wrote it owns the names it declared
//!     either way.
//!
//! Runs on SQLite with no server: ownership is decided during lowering.

mod support;

use std::collections::BTreeMap;

use zero_migrate::{
    resolve_create_table_policy, GuardConfig, IrAuthor, LiveSchema, MigrationIr, SqlDialect,
};

const PROJECT: &str = "app";
const APP: &str = "app";

fn artifact_created_tables(ops_json: &str) -> Vec<String> {
    let raw = format!(r#"{{"ir_version":1,"name":"legs","ops":{ops_json}}}"#);
    let ir: MigrationIr = serde_json::from_str(&raw).expect("the test IR parses");
    let resolved = resolve_create_table_policy(&ir, &support::no_inject("app"), PROJECT)
        .expect("the test IR resolves without platform columns");
    let resolved_json = serde_json::to_string(&resolved).expect("resolved test IR serializes");

    IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &support::no_inject("app"))
        .load_and_lower_guarded(
            &resolved_json,
            APP,
            &BTreeMap::new(),
            &LiveSchema::default(),
            &GuardConfig::from_policy(support::no_inject(PROJECT), SqlDialect::Sqlite),
        )
        .expect("the dialectal create lowers under SQLite")
        .created_tables
}

/// The control: a top-level create is claimed. Without this the arms below could pass
/// on a `created_tables` that is broken for every shape.
#[test]
fn a_top_level_create_is_registered_as_owned() {
    let created = artifact_created_tables(
        r#"[{"op":"createTable","name":"plain","columns":[
              {"name":"title","type":"text","nullable":true}]}]"#,
    );
    assert!(
        created.iter().any(|table| table == "plain"),
        "a top-level createTable claims its name: {created:?}"
    );
}

/// The reported defect: the SQLite leg is the SELECTED one here, so `wrapped` really is
/// created on this database - and it was left unclaimed.
#[test]
fn a_create_in_the_selected_leg_is_registered_as_owned() {
    let created = artifact_created_tables(
        r#"[{"op":"dialectal","sqlite":[
              {"op":"createTable","name":"wrapped","columns":[
                {"name":"title","type":"text","nullable":true}]}]}]"#,
    );
    assert!(
        created.iter().any(|table| table == "wrapped"),
        "SQLite selected this leg, so `wrapped` exists on the database and must be \
         owned: {created:?}"
    );
}

/// The all-legs half. `only_pg` is not created on SQLite, but the app that authored it
/// still owns the name - matching the load gate, and failing closed if the choice is
/// ever wrong.
#[test]
fn a_create_in_an_unselected_leg_is_registered_as_owned_too() {
    let created = artifact_created_tables(
        r#"[{"op":"dialectal","pg":[
              {"op":"createTable","name":"only_pg","columns":[
                {"name":"title","type":"text","nullable":true}]}]}]"#,
    );
    assert!(
        created.iter().any(|table| table == "only_pg"),
        "the authoring app owns every name it declared, on any target: {created:?}"
    );
}
