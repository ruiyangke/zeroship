//! The FIELD-DEF carrier's MySQL type map, one line per type token, and the collation
//! each one carries.
//!
//! The engine has two MySQL renderers. `render::declarative` answers from a
//! [`FieldDescriptor`] and a PostgreSQL-spelled `data_type` (the SNAPSHOT carrier);
//! `schema::query::renderer(SqlDialect::Mysql)` answers from a raw SDK field def (the
//! FIELD-DEF carrier). Both decide what a MySQL column is, and only the first pinned a
//! collation - so on the second one every character column inherited the table
//! default, which on a stock MySQL 8 server is `utf8mb4_0900_ai_ci`.
//!
//! # A SPELLING PIN, and what that does and does not buy
//!
//! Everything below compares one function in this repo against a literal in this
//! repo, which proves nothing about what a server does. The server half lives in
//! `tests/mysql_engine/mysql_query_renderer_collation.rs`, which hands MySQL the
//! statements this map renders and measures whether `'Active' = 'active'` and whether
//! a UNIQUE index collapses the two. Deliberately a different test binary: the
//! measurement and the enumeration answer different questions and fail for different
//! reasons.
//!
//! What this file adds is COVERAGE OF THE WHOLE SURFACE. The live file probes seven
//! character spellings on a server; this one names every token the map can be handed,
//! so an arm added later that quietly lands on the uncollated fallback fails here.
//!
//! # And what nothing here proves: nothing reaches this arm today
//!
//! Measured, not assumed. `MysqlSchemaRenderer::column_type` was given a tripwire that
//! panics on entry and the whole Rust suite was run against live PostgreSQL, MySQL and
//! SQLite (37 sections, 3333 tests): exactly eight tests tripped it, all eight
//! `#[cfg(test)]` unit tests inside `schema/query.rs` itself. The dialect-generic
//! emitter's only production caller is the SQLite 12-step rebuild, which passes a
//! hardcoded `SqlDialect::Sqlite`; the `def_to_column_type_for_dialect` call sites in
//! `render::declarative` and `schema::diff` all pass a hardcoded `SqlDialect::Postgres`.
//!
//! So this pin is the whole of this arm's coverage together with the live file, and no
//! deployment moves if it regresses. It is here because `schema::query` is a `pub mod`
//! and `def_to_column_type_for_dialect` takes the dialect as a PARAMETER: the first
//! caller that passes `Mysql` gets whatever this map says.

use crate::support;

use std::collections::HashMap;

use zero_migrate::model::snapshot::SchemaSnapshot;
use zero_migrate::render::declarative::{
    desired_snapshot_for_dialect, CollectionDescriptor, DeclarativeAuthor, FieldDescriptor,
};
use zero_migrate::schema::query::def_to_column_type_for_dialect;
use zero_migrate::SqlDialect;

const CASE_SENSITIVE: &str = "CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_as_cs";
const CASE_INSENSITIVE: &str = "CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci";

fn mysql_type(def: serde_json::Value) -> String {
    def_to_column_type_for_dialect(&def, SqlDialect::Mysql)
}

/// EVERY character spelling the field-def carrier can produce, named rather than
/// counted, with the collation each must carry.
///
/// The list is the answer to "which types were affected": `string` in all three of its
/// outcomes, `char` in both, `ref`, `inet`, a string-valued `literal`, the native
/// `ENUM`, and the unknown-token fallback that every unrecognised type lands on. Miss
/// one and its columns silently compare case-insensitively on MySQL.
#[test]
fn every_character_spelling_pins_the_case_sensitive_collation() {
    let cases: Vec<(&str, serde_json::Value, String)> = vec![
        (
            "string with a bound",
            serde_json::json!({ "type": "string", "maxLength": 64 }),
            format!("VARCHAR(64) {CASE_SENSITIVE}"),
        ),
        (
            "string bounded past the VARCHAR row limit",
            serde_json::json!({ "type": "string", "maxLength": 40000 }),
            format!("LONGTEXT {CASE_SENSITIVE}"),
        ),
        (
            "string with no bound",
            serde_json::json!({ "type": "string" }),
            format!("VARCHAR(191) {CASE_SENSITIVE}"),
        ),
        (
            "char with a length",
            serde_json::json!({ "type": "char", "charLen": 8 }),
            format!("CHAR(8) {CASE_SENSITIVE}"),
        ),
        (
            "char with no length",
            serde_json::json!({ "type": "char" }),
            format!("CHAR(1) {CASE_SENSITIVE}"),
        ),
        (
            "ref",
            serde_json::json!({ "type": "ref" }),
            format!("VARCHAR(191) {CASE_SENSITIVE}"),
        ),
        (
            "inet",
            serde_json::json!({ "type": "inet" }),
            format!("VARCHAR(43) {CASE_SENSITIVE}"),
        ),
        (
            "literal holding a string",
            serde_json::json!({ "type": "literal", "literalValue": "draft" }),
            format!("VARCHAR(191) {CASE_SENSITIVE}"),
        ),
        (
            "the unknown-token fallback",
            serde_json::json!({ "type": "notAThingTheEngineKnows" }),
            format!("VARCHAR(191) {CASE_SENSITIVE}"),
        ),
        (
            // MySQL treats ENUM as a character type: member LOOKUP runs under the
            // column's collation, so an uncollated one accepts 'ACTIVE' for a declared
            // 'active'. Measured in `tests/mysql_engine/mysql_enum_collation.rs`.
            "native enum",
            serde_json::json!({ "type": "string", "enum": ["active", "archived"] }),
            // The members cross as hex literals rather than quoted strings: MySQL's
            // ENUM grammar takes a bare hex literal, and that spelling is independent
            // of `NO_BACKSLASH_ESCAPES`. `616374697665` is `active`,
            // `6172636869766564` is `archived`.
            format!("ENUM(X'616374697665', X'6172636869766564') {CASE_SENSITIVE}"),
        ),
    ];

    for (label, def, expected) in cases {
        assert_eq!(mysql_type(def), expected, "{label}");
    }
}

/// The other direction. A `caseSensitive: false` field must reach the case-INSENSITIVE
/// collation rather than simply losing its pin, which is why the emitter reads the
/// facet instead of hard-coding one collation.
#[test]
fn a_case_insensitive_field_pins_the_case_insensitive_collation() {
    assert_eq!(
        mysql_type(serde_json::json!({
            "type": "string", "maxLength": 64, "caseSensitive": false
        })),
        format!("VARCHAR(64) {CASE_INSENSITIVE}")
    );
    assert_eq!(
        mysql_type(serde_json::json!({ "type": "char", "charLen": 8, "caseSensitive": false })),
        format!("CHAR(8) {CASE_INSENSITIVE}")
    );
}

/// The half that must NOT move, and it is not cosmetic: `JSON COLLATE ...` is a parse
/// error, not a redundancy, so a pin that reached one of these would take the whole
/// `CREATE TABLE` down.
#[test]
fn no_non_character_spelling_takes_a_collation() {
    let cases: Vec<(&str, serde_json::Value, &str)> = vec![
        ("integer", serde_json::json!({ "type": "integer" }), "INT"),
        ("bigInt", serde_json::json!({ "type": "bigInt" }), "BIGINT"),
        (
            "smallInt",
            serde_json::json!({ "type": "smallInt" }),
            "SMALLINT",
        ),
        ("number", serde_json::json!({ "type": "number" }), "DOUBLE"),
        (
            "number carrying precision",
            serde_json::json!({ "type": "number", "precision": 20, "scale": 4 }),
            "DECIMAL(20, 4)",
        ),
        ("real", serde_json::json!({ "type": "real" }), "FLOAT"),
        (
            "boolean",
            serde_json::json!({ "type": "boolean" }),
            "TINYINT(1)",
        ),
        ("date", serde_json::json!({ "type": "date" }), "DATETIME(6)"),
        (
            "calendarDate",
            serde_json::json!({ "type": "calendarDate" }),
            "DATE",
        ),
        ("json", serde_json::json!({ "type": "json" }), "JSON"),
        ("object", serde_json::json!({ "type": "object" }), "JSON"),
        ("array", serde_json::json!({ "type": "array" }), "JSON"),
        ("union", serde_json::json!({ "type": "union" }), "JSON"),
        (
            "textArray",
            serde_json::json!({ "type": "textArray" }),
            "JSON",
        ),
        ("bytes", serde_json::json!({ "type": "bytes" }), "LONGBLOB"),
        (
            "encrypted",
            serde_json::json!({ "type": "string", "encrypted": { "mode": "deterministic" } }),
            "LONGBLOB",
        ),
        ("vector", serde_json::json!({ "type": "vector" }), "BLOB"),
        (
            "geoPoint",
            serde_json::json!({ "type": "geoPoint" }),
            "POINT SRID 4326",
        ),
        (
            "literal holding a number",
            serde_json::json!({ "type": "literal", "literalValue": 2.5 }),
            "DECIMAL(65, 30)",
        ),
        (
            "literal holding a bool",
            serde_json::json!({ "type": "literal", "literalValue": true }),
            "TINYINT(1)",
        ),
    ];

    for (label, def, expected) in cases {
        assert_eq!(mysql_type(def), expected, "{label}");
    }
}

/// The two carriers, asked about the SAME authored column, must pin the SAME
/// collation.
///
/// This is the assertion the whole change exists for, and it compares two renderers
/// rather than a renderer against a literal - so a future edit that moves the
/// engine's collation choice has to move BOTH or fail here.
#[test]
fn both_mysql_carriers_pin_the_same_collation_for_the_same_column() {
    let field_def = mysql_type(serde_json::json!({ "type": "string", "maxLength": 64 }));
    let snapshot_ddl = descriptor_create_ddl().expect("the descriptor plans a MySQL CREATE TABLE");

    assert!(
        field_def.contains(CASE_SENSITIVE),
        "the field-def carrier answered {field_def:?}, which does not pin {CASE_SENSITIVE}"
    );
    assert!(
        snapshot_ddl.contains(CASE_SENSITIVE),
        "the snapshot carrier answered:\n{snapshot_ddl}\nwhich does not pin {CASE_SENSITIVE}"
    );
    assert!(
        !field_def.contains(CASE_INSENSITIVE) && !snapshot_ddl.contains(CASE_INSENSITIVE),
        "neither carrier may reach the case-INSENSITIVE collation for a column that did \
         not ask for it:\nfield-def: {field_def}\nsnapshot:\n{snapshot_ddl}"
    );

    // The one place the two carriers DO still differ, recorded rather than asserted
    // away: the snapshot carrier spells a `caseSensitive: false` bounded string as
    // `TEXT` (`render::declarative::mysql_base_column_type` routes it through the
    // PostgreSQL `citext`/`text` mapping and loses the bound), while the field-def
    // carrier keeps `VARCHAR(n)`. That is a BASE-SPELLING divergence, not a collation
    // one, it predates this change, and nothing in production reaches the field-def
    // carrier's MySQL arm to be affected by it. Closing it would change storage, so it
    // is deliberately out of scope here.
    let ci = mysql_type(serde_json::json!({
        "type": "string", "maxLength": 64, "caseSensitive": false
    }));
    assert_eq!(
        ci,
        format!("VARCHAR(64) {CASE_INSENSITIVE}"),
        "the field-def carrier keeps the bound on a case-insensitive string; if this \
         ever becomes TEXT the two carriers have converged and this note is stale"
    );
}

/// The MySQL `CREATE TABLE` the SNAPSHOT carrier plans for one bounded-string column
/// on a first deploy.
fn descriptor_create_ddl() -> Result<String, String> {
    const PROJECT: &str = "app_mysql_carrier_collation";
    let descriptor = CollectionDescriptor {
        name: "notes".to_string(),
        owner_app: PROJECT.to_string(),
        fields: vec![FieldDescriptor {
            name: "title".to_string(),
            ty: "string".to_string(),
            required: true,
            max_length: Some(64),
            ..Default::default()
        }],
        indexes: vec![],
        runtime_options: Default::default(),
    };
    let effective = support::no_inject(PROJECT);
    let desired = desired_snapshot_for_dialect(
        PROJECT,
        std::slice::from_ref(&descriptor),
        SqlDialect::Mysql,
        &effective,
    )
    .map_err(|e| format!("build the desired snapshot: {e}"))?;
    DeclarativeAuthor::new_for_dialect(PROJECT, PROJECT, SqlDialect::Mysql)
        .diff(
            &desired,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &[],
            &effective,
        )
        .map_err(|e| format!("diff against an empty live schema: {e}"))?
        .migrations
        .iter()
        .map(|m| m.up.clone())
        .find(|up| up.contains("CREATE TABLE"))
        .ok_or_else(|| "the first-deploy plan carried no CREATE TABLE".to_string())
}
