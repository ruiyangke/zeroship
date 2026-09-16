//! **`schema.runtime.json` sees inside the selected dialect leg.**
//!
//! `render_artifacts` builds its projections from the SAME op stream, and all
//! three - `FoldedSchema::project_field_defs`,
//! `FoldedSchema::project_authoring_tables` and
//! `FoldedSchema::project_runtime_metadata` - read ONE traversal that
//! `flatten_dialectal_ops` opens. So COLUMNS, runtime OPTIONS and plain INDEXES
//! authored inside a `dialect()` leg must all reach the artifact: the wrapper is
//! transparent to the projection, never a fall-through.
//!
//! The rule this file pins: the artifact must never describe a table the database
//! does not have - a collection whose fields are present and whose index is
//! missing, on the very dialect whose leg declared it. Nothing else covers the
//! map - `render_runtime_descriptor_v2` takes it as given and falls back to
//! `unwrap_or_default()` for a collection it lacks, so an absent entry is
//! indistinguishable from a table that genuinely declared nothing.
//!
//! The arms below pin the rule from both sides, because a projection that simply
//! unioned every leg would pass the first arm and be just as wrong: an index
//! declared only in the SQLite leg must NOT appear in the PostgreSQL artifact.
//! Selection, not union.
//!
//! No live database is needed. The rule lives entirely in the offline projection,
//! and the oracle is the emitted artifact rather than a catalog.

use crate::support;

use serde_json::Value;

use zeroship_migrate::model::ir::{MigrationIr, Op};
use zeroship_migrate::render_artifacts;

const SCHEMA: &str = "public";

/// `notes(id, body)` at the top level, then a `dialect()` wrapper whose PostgreSQL leg
/// declares a plain index and turns a runtime option on, and whose SQLite leg declares
/// a DIFFERENT index. The table itself is unconditional so both arms describe the same
/// collection and differ only in what the leg contributed.
fn history() -> Vec<Op> {
    let source = r#"{
  "ir_version": 1,
  "name": "dialectal_runtime_metadata",
  "owner_app": "app_test",
  "ops": [
    {"op":"createTable","name":"notes","columns":[
      {"name":"id","type":"int","nullable":false},
      {"name":"body","type":"text","nullable":true}
    ],"primaryKey":["id"]},
    {"op":"dialectal",
     "legs":{
       "postgres":[
         {"op":"createIndex","table":"notes","name":"notes_pg_idx",
          "columns":[{"kind":"column","name":"body"}]},
         {"op":"setTableOptions","table":"notes","options":{"softDelete":true}}
       ],
       "sqlite":[
         {"op":"createIndex","table":"notes","name":"notes_sqlite_idx",
          "columns":[{"kind":"column","name":"body"}]}
       ]}}
  ]
}"#;
    serde_json::from_str::<MigrationIr>(source)
        .expect("the dialectal runtime-metadata IR parses")
        .ops
}

fn runtime_json(dialect: &zeroship_migrate::DialectId) -> Value {
    let (ops, policy) = support::lifecycle_fixture(&history(), SCHEMA);
    let artifacts = render_artifacts(
        zeroship_migrate::shipping_vendors(),
        &ops,
        dialect,
        SCHEMA,
        &policy,
    )
    .expect("the dialectal history renders artifacts");
    serde_json::from_str(&artifacts.runtime_json).expect("schema.runtime.json parses")
}

/// The `notes` collection object out of the v2 descriptor, so a failure prints the
/// collection rather than the whole document.
fn notes(dialect: &zeroship_migrate::DialectId) -> Value {
    let doc = runtime_json(dialect);
    doc.get("collections")
        .and_then(|c| c.get("notes"))
        .unwrap_or_else(|| panic!("the descriptor should carry the `notes` collection: {doc:#}"))
        .clone()
}

/// An index authored inside the SELECTED leg is part of the table PostgreSQL
/// gets, so the artifact has to name it.
#[test]
fn an_index_authored_in_the_selected_leg_reaches_the_runtime_descriptor() {
    let collection = notes(&zeroship_migrate_postgres::DIALECT);
    let rendered = serde_json::to_string(&collection).expect("collection serializes");
    assert!(
        rendered.contains("notes_pg_idx"),
        "the PostgreSQL leg declared `notes_pg_idx`, so PostgreSQL creates it and the \
         artifact must name it: {collection:#}"
    );
}

/// The same rule on the other kind of metadata the projection owns. A runtime
/// option set inside the selected leg changes how the collection behaves at
/// runtime, and an absent map entry silently reads as the default rather than as
/// unknown.
#[test]
fn a_runtime_option_set_in_the_selected_leg_reaches_the_runtime_descriptor() {
    let collection = notes(&zeroship_migrate_postgres::DIALECT);
    assert_eq!(
        collection.pointer("/options/softDelete"),
        Some(&Value::Bool(true)),
        "the PostgreSQL leg turned `softDelete` on, so the artifact must carry it \
         rather than falling back to the default: {collection:#}"
    );
}

/// The control that keeps the rule SELECTION rather than union. `notes_sqlite_idx`
/// is declared only in the inactive leg, so PostgreSQL never creates it and an
/// artifact naming it would describe an index the database does not have. A
/// projection that unioned every leg instead of selecting one passes the arms
/// above and fails here.
#[test]
fn an_index_authored_only_in_an_inactive_leg_stays_out_of_the_artifact() {
    let collection = notes(&zeroship_migrate_postgres::DIALECT);
    let rendered = serde_json::to_string(&collection).expect("collection serializes");
    assert!(
        !rendered.contains("notes_sqlite_idx"),
        "`notes_sqlite_idx` exists only in the SQLite leg, so PostgreSQL does not \
         create it and the artifact must not claim it: {collection:#}"
    );
}

/// The mirror of the control, run under SQLite: the same history has to produce the
/// SQLite leg's index and not the PostgreSQL one. Without this arm a projection
/// hardcoded to the PostgreSQL leg would pass every arm above.
#[test]
fn the_sqlite_artifact_carries_the_sqlite_leg_and_not_the_postgres_one() {
    let collection = notes(&zeroship_migrate_sqlite::DIALECT);
    let rendered = serde_json::to_string(&collection).expect("collection serializes");
    assert!(
        rendered.contains("notes_sqlite_idx"),
        "the SQLite leg declared `notes_sqlite_idx`: {collection:#}"
    );
    assert!(
        !rendered.contains("notes_pg_idx"),
        "`notes_pg_idx` exists only in the PostgreSQL leg: {collection:#}"
    );
}
