//! What a `setColumnType` does to every OTHER facet the column carries, held to
//! ONE answer across the three replays of the same op stream.
//!
//! The three replays and their artifacts:
//!
//! | replay                      | artifact                                    |
//! |-----------------------------|---------------------------------------------|
//! | the authoring tables *      | `env.db.ts`                                 |
//! | the `FieldDef` map *        | `schema.runtime.json` AND, on SQLite, the   |
//! |                             | DESIRED snapshot the 12-step rebuild        |
//! |                             | renders `CREATE TABLE` from                 |
//! | `fold_ops`                  | the snapshot drift compares                 |
//!
//! \* both were private walkers until step 4 of
//! `docs/proposals/single-fold-and-effects.md`: `authoring_tables_from_ops` until
//! consumer 2 and `fold_to_field_defs` until consumer 3. They are
//! `FoldedSchema::project_authoring_tables` and `FoldedSchema::project_field_defs` now,
//! and - the point of the proposal - they are two READS of ONE traversal, so the top two
//! rows of that table are no longer two replays that can disagree. The retype verdict
//! they must agree on lives on that traversal's `Op::SetColumnType` arm; it lived on
//! `render::lower::retype_field_descriptor` until consumer 3 deleted it, the walker
//! having been its only caller.
//!
//! This file drives `render_artifacts`, so it measures whichever producer is wired in
//! and needed no change across either move - which is itself the claim it makes about
//! the retype verdict surviving a producer swap.
//!
//! They disagreed. `fold_to_field_defs` replayed a retype by assigning the TYPE
//! TOKEN and nothing else, which is wrong in BOTH directions because the token is
//! not the whole type: `ColType::String { length }`, `Char { length }` and
//! `Vector { vector }` carry their parameter in a SIBLING descriptor field.
//! Measured before this fixture existed, by folding each envelope and printing all
//! three replays:
//!
//! ```text
//!                            fold_to_field_defs      env.db.ts        fold_ops
//!   string(24) -> int        maxLength: 24 STALE     t.int()          integer
//!   string(24) -> string(40) maxLength: 24 WRONG     length: 40       varchar(40)
//!   int -> string(40)        maxLength ABSENT        length: 40       varchar(40)
//!   int -> char(8)           charLen ABSENT          length: 8        character(8)
//!   int -> vector(3)         vectorDims ABSENT       dimensions: 3    vector(3)
//!   vector(3) -> vector(5)   vectorDims: 3 WRONG     dimensions: 5    vector(5)
//!   text(ci) -> int          caseSensitive STALE     t.int()          case_sensitive STALE
//! ```
//!
//! The last row is the one where `fold_ops` is wrong too, and it is not cosmetic:
//! `case_sensitive` is DRIFT-COMPARED. So is `collation`, `value_format` and
//! `id_default`, and `fold_ops` left all four behind.
//!
//! THE VERDICT PER FACET is written where it is enforced -
//! [`zero_migrate::render::lower::retype_field_descriptor`] holds the table and the
//! reason for each entry, measured against live PostgreSQL 18.4. This fixture
//! pins the OBSERVABLE half of it, and pins the three replays to each other so a
//! future divergence is a test failure rather than a discovery.

use crate::support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::render::fold::fold_ops;
use zero_migrate::render::fold::single_fold;
use zero_migrate::schema::query::SqlDialect;

const SCHEMA: &str = "public";

/// One column `v` beside a `c0 int` primary key, then one `setColumnType` on it.
fn envelope(create_col: &str, to_type: &str) -> MigrationIr {
    let bytes = format!(
        r#"{{"ir_version":1,"name":"n","ops":[
            {{"op":"createTable","name":"a","columns":[
                {{"name":"c0","type":"int","nullable":false}},
                {create_col}
            ],"primaryKey":["c0"]}},
            {{"op":"setColumnType","table":"a","column":"v","toType":{to_type}}}
        ]}}"#
    );
    serde_json::from_str(&bytes).expect("the envelope parses")
}

/// What the `FieldDef` projection says column `v` is - the `schema.runtime.json` entry
/// and, on SQLite, the desired-snapshot input for the rebuild.
fn descriptor(create_col: &str, to_type: &str) -> serde_json::Value {
    let ir = envelope(create_col, to_type);
    let effective = support::operator_charter(SCHEMA);
    single_fold::fold(&ir.ops, SqlDialect::Postgres, SCHEMA, &effective)
        .map(|folded| folded.project_field_defs())
        .expect("the descriptor fold succeeds")
        .get("a")
        .and_then(|table| table.get("v"))
        .cloned()
        .expect("table a has column v")
}

/// The `env.db.ts` line for column `v`, from whichever producer `render_artifacts`
/// is wired to - `FoldedSchema::project_authoring_tables` since step 4 consumer 2.
fn authoring(create_col: &str, to_type: &str) -> String {
    let ir = envelope(create_col, to_type);
    let effective = support::operator_charter(SCHEMA);
    zero_migrate::render::gen_types::render_artifacts(
        &ir.ops,
        SqlDialect::Postgres,
        SCHEMA,
        &effective,
    )
    .expect("the authoring replay succeeds")
    .env_db_ts
    .lines()
    .find(|line| line.trim_start().starts_with("v:"))
    .expect("env.db.ts declares column v")
    .trim()
    .to_string()
}

/// The `ColumnSnapshot` `fold_ops` folds for column `v`, on `dialect`.
fn snapshot(
    dialect: SqlDialect,
    project: &str,
    create_col: &str,
    to_type: &str,
) -> Result<zero_migrate::model::snapshot::ColumnSnapshot, String> {
    let ir = envelope(create_col, to_type);
    let effective = support::operator_charter(project);
    let folded =
        fold_ops(&ir.ops, dialect, project, &effective).map_err(|error| error.to_string())?;
    folded
        .tables
        .get("a")
        .and_then(|table| table.columns.iter().find(|c| c.name == "v"))
        .cloned()
        .ok_or_else(|| "table a has no column v".to_string())
}

fn pg_snapshot(
    create_col: &str,
    to_type: &str,
) -> Result<zero_migrate::model::snapshot::ColumnSnapshot, String> {
    snapshot(SqlDialect::Postgres, SCHEMA, create_col, to_type)
}

const CI_TEXT: &str = r#"{"name":"v","type":"text","caseSensitive":false}"#;
const STRING_24: &str = r#"{"name":"v","type":{"string":{"length":24}}}"#;
const CHAR_8: &str = r#"{"name":"v","type":{"char":{"length":8}}}"#;
const VECTOR_3: &str = r#"{"name":"v","type":{"vector":{"vector":3}},"vectorMetric":"cosine"}"#;
const PLAIN_INT: &str = r#"{"name":"v","type":"int"}"#;
const TYPE_ID: &str = r#"{"name":"v","type":"text","valueFormat":{"typeId":{"prefix":"usr"}}}"#;
const ULID: &str = r#"{"name":"v","type":"text","valueFormat":"ulid"}"#;

const TO_INT: &str = r#""int""#;
const TO_STRING_40: &str = r#"{"string":{"length":40}}"#;
const TO_CHAR_8: &str = r#"{"char":{"length":8}}"#;
const TO_VECTOR_5: &str = r#"{"vector":{"vector":5}}"#;

// ---------------------------------------------------------------------------
// RE-DERIVED FROM THE TARGET TYPE. `ColType` carries the parameter, so the only
// truthful answer is the one the new type gives - in both directions.
// ---------------------------------------------------------------------------

#[test]
fn a_retype_off_a_bounded_string_drops_the_bound_it_no_longer_has() {
    assert_eq!(
        descriptor(STRING_24, TO_INT),
        serde_json::json!({ "type": "int" }),
        "`maxLength` is `ColType::String`'s parameter. An int has no length, and \
         `env.db.ts` (`t.int()`) and the snapshot (`integer`) both already say so"
    );
    assert_eq!(authoring(STRING_24, TO_INT), "v: t.int(),");
    assert_eq!(
        pg_snapshot(STRING_24, TO_INT).expect("fold").data_type,
        "integer"
    );
}

#[test]
fn a_retype_between_two_bounded_strings_reports_the_new_bound() {
    // The sharpest of the set: the type TOKEN does not change (`ColType::String`
    // and `ColType::String` are both the `string` token), so a replay that
    // assigns only the token cannot tell this case from a no-op. The descriptor
    // said 24 while the database held 40.
    assert_eq!(
        descriptor(STRING_24, TO_STRING_40),
        serde_json::json!({ "type": "string", "maxLength": 40 }),
    );
    assert_eq!(
        authoring(STRING_24, TO_STRING_40),
        "v: t.string({ length: 40 }),"
    );
    assert_eq!(
        pg_snapshot(STRING_24, TO_STRING_40)
            .expect("fold")
            .data_type,
        "character varying(40)"
    );
}

#[test]
fn a_retype_into_a_parameterised_type_reports_the_parameter() {
    // The ABSENT direction, which is worse than stale residue: `{"type":"char"}`
    // with no `charLen` is not a type. On SQLite this map IS the desired snapshot
    // the 12-step rebuild renders its `CREATE TABLE` from.
    assert_eq!(
        descriptor(PLAIN_INT, TO_STRING_40),
        serde_json::json!({ "type": "string", "maxLength": 40 }),
    );
    assert_eq!(
        descriptor(PLAIN_INT, TO_CHAR_8),
        serde_json::json!({ "type": "char", "charLen": 8 }),
    );
    assert_eq!(
        descriptor(PLAIN_INT, r#"{"vector":{"vector":3}}"#),
        serde_json::json!({ "type": "vector", "vectorDims": 3 }),
        "a dimensionless `vector` is not a pgvector type"
    );
}

#[test]
fn a_retype_between_two_vector_widths_reports_the_new_width() {
    assert_eq!(
        descriptor(VECTOR_3, TO_VECTOR_5),
        serde_json::json!({ "type": "vector", "vectorDims": 5 }),
        "`vectorMetric` is cleared with the rest of the vector facets: \
         `setColumnType` has no slot to re-declare it"
    );
    assert_eq!(
        pg_snapshot(VECTOR_3, TO_VECTOR_5).expect("fold").data_type,
        "vector(5)"
    );
}

#[test]
fn a_retype_off_a_char_column_drops_its_fixed_length() {
    assert_eq!(
        descriptor(CHAR_8, TO_INT),
        serde_json::json!({ "type": "int" }),
    );
}

#[test]
fn a_retype_off_a_vector_column_drops_its_dimensionality_and_metric() {
    assert_eq!(
        descriptor(VECTOR_3, TO_INT),
        serde_json::json!({ "type": "int" }),
        "`vectorDims` and `vectorMetric` are both meaningless off a vector type; \
         the metric is a DECLARED-ONLY opclass selector with no catalog trace"
    );
}

// ---------------------------------------------------------------------------
// CLEARED. `Op::SetColumnType` has no slot to re-declare these, so the only
// state a retype can leave them in is absent - and the SERVER agrees.
// ---------------------------------------------------------------------------

#[test]
fn a_retype_clears_case_insensitivity_in_all_three_replays() {
    // Measured on live PostgreSQL 18.4: case-insensitivity IS the `citext` TYPE,
    // so `ALTER ... TYPE character varying(40)` leaves a plain, case-SENSITIVE
    // column. `case_sensitive` is drift-compared, so keeping it reports a
    // difference that does not exist - see `set_column_type_facets_pg`.
    assert_eq!(
        descriptor(CI_TEXT, TO_INT),
        serde_json::json!({ "type": "int" }),
    );
    assert_eq!(authoring(CI_TEXT, TO_INT), "v: t.int(),");
    assert_eq!(
        pg_snapshot(CI_TEXT, TO_INT).expect("fold").case_sensitive,
        None,
    );
    // And on the retype that PostgreSQL ACCEPTS, where the stale facet actually
    // reaches a live comparison.
    assert_eq!(
        descriptor(CI_TEXT, TO_STRING_40),
        serde_json::json!({ "type": "string", "maxLength": 40 }),
    );
    assert_eq!(
        pg_snapshot(CI_TEXT, TO_STRING_40)
            .expect("fold")
            .case_sensitive,
        None,
    );
}

#[test]
fn the_only_fold_side_producer_of_a_collation_is_the_facet_the_retype_refuses() {
    // `collation` is DRIFT-COMPARED and PostgreSQL RESETS it on ALTER TYPE
    // (measured: `text COLLATE "C" -> character varying(40)` leaves the catalog
    // reporting the DEFAULT collation, never `C`). It was reachable exactly once,
    // through a value-format column - `render::value_format`'s
    // `bytewise_catalog_collation` is the ONE fold-side writer, and it runs only
    // for TypeID/ULID. SQLite's `NOCASE` rides on `case_sensitive`, not here.
    //
    // So the refusal above closes the only route, and `fold_ops`'s
    // `col.collation = new_col.collation` is belt-and-braces rather than the fix.
    // This test states that rather than asserting `None == None` on a column that
    // never had a collation, which would pass against ANY implementation.
    assert!(pg_snapshot(TYPE_ID, r#""text""#)
        .expect_err("the one route to a collation is refused")
        .contains("value format"));
    // And a column that carries no collation to begin with still carries none.
    assert_eq!(pg_snapshot(CI_TEXT, TO_INT).expect("fold").collation, None);
}

#[test]
fn a_retype_off_an_enum_column_drops_the_enum_check_it_left_behind() {
    // `inline_checks` is emission-only, but it is DDL: the SQLite rebuild joins
    // it straight into the new table's column declaration. Before the fix, a
    // SQLite `enum -> int` retype left `CHECK ("v" IN ('ok', 'bad'))` sitting on
    // an `integer` column - the same shape as the stale generated body that made
    // a SQLite rename undeployable.
    let ir: MigrationIr = serde_json::from_str(
        r#"{"ir_version":1,"name":"n","ops":[
            {"op":"createEnum","name":"mood","values":["ok","bad"]},
            {"op":"createTable","name":"a","columns":[
                {"name":"c0","type":"int","nullable":false},
                {"name":"v","type":{"enum":{"name":"mood"}}}
            ],"primaryKey":["c0"]},
            {"op":"setColumnType","table":"a","column":"v","toType":"int"}
        ]}"#,
    )
    .expect("the envelope parses");
    let effective = support::operator_charter("main");
    let folded = fold_ops(&ir.ops, SqlDialect::Sqlite, "main", &effective).expect("fold");
    let column = folded
        .tables
        .get("a")
        .and_then(|table| table.columns.iter().find(|c| c.name == "v"))
        .expect("column v");
    assert_eq!(column.data_type, "integer");
    assert_eq!(
        column.inline_checks,
        Vec::<String>::new(),
        "the enum's membership CHECK belongs to the type the column no longer has"
    );
}

// ---------------------------------------------------------------------------
// KEPT. Orthogonal to storage, and PostgreSQL carries each one across
// `ALTER COLUMN ... TYPE` - measured, not assumed.
// ---------------------------------------------------------------------------

#[test]
fn a_retype_keeps_the_facets_the_server_carries_across_the_alter() {
    // NOT NULL: `attnotnull` still `t` after `ALTER ... TYPE integer`.
    let not_null = r#"{"name":"v","type":"text","nullable":false}"#;
    assert_eq!(
        descriptor(not_null, TO_INT),
        serde_json::json!({ "type": "int", "required": true }),
    );
    assert!(!pg_snapshot(not_null, TO_INT).expect("fold").nullable);

    // UNIQUE: PostgreSQL rebuilds the unique index and it survives.
    let unique = r#"{"name":"v","type":"text","unique":true}"#;
    assert_eq!(
        descriptor(unique, TO_INT),
        serde_json::json!({ "type": "int", "unique": true }),
    );

    // DEFAULT: PostgreSQL re-casts it, and REFUSES the whole ALTER when it
    // cannot - so a default that survives to the fold is one the server kept.
    let defaulted = r#"{"name":"v","type":"int","default":{"literal":{"value":7}}}"#;
    assert_eq!(
        descriptor(defaulted, r#""bigInt""#),
        serde_json::json!({ "type": "bigInt", "default": 7 }),
    );
    assert_eq!(
        pg_snapshot(defaulted, r#""bigInt""#)
            .expect("fold")
            .default
            .as_deref(),
        Some("7"),
    );
}

#[test]
fn a_retype_keeps_a_generated_body_and_an_identity_the_server_keeps() {
    // `attgenerated` and `attidentity` both survive a compatible ALTER TYPE.
    let generated = r#"{"name":"v","type":"int","generated":{"expr":{"node":"binOp","op":"add","lhs":{"node":"colRef","name":"c0"},"rhs":{"node":"literal","value":{"int64":"1"}}},"stored":true}}"#;
    let folded = pg_snapshot(generated, r#""bigInt""#).expect("fold");
    assert_eq!(folded.data_type, "bigint");
    assert_eq!(
        folded
            .generated
            .as_ref()
            .map(|generated| generated.expr.as_str()),
        Some("(\"c0\" + 1)"),
    );
    assert!(descriptor(generated, r#""bigInt""#)
        .get("generated")
        .is_some());

    let identity = r#"{"name":"v","type":"int","nullable":false,"identity":{"always":false}}"#;
    let folded = pg_snapshot(identity, r#""bigInt""#).expect("fold");
    assert_eq!(folded.data_type, "bigint");
    assert!(folded.identity.is_some());
}

#[test]
fn a_retype_keeps_a_user_comment_the_alter_does_not_touch() {
    let ir: MigrationIr = serde_json::from_str(
        r#"{"ir_version":1,"name":"n","ops":[
            {"op":"createTable","name":"a","columns":[
                {"name":"c0","type":"int","nullable":false},
                {"name":"v","type":"text"}
            ],"primaryKey":["c0"]},
            {"op":"comment","target":{"kind":"column","table":"a","name":"v"},"comment":"a note"},
            {"op":"setColumnType","table":"a","column":"v","toType":"int"}
        ]}"#,
    )
    .expect("the envelope parses");
    let effective = support::operator_charter(SCHEMA);
    let folded = fold_ops(&ir.ops, SqlDialect::Postgres, SCHEMA, &effective).expect("fold");
    let column = folded
        .tables
        .get("a")
        .and_then(|table| table.columns.iter().find(|c| c.name == "v"))
        .expect("column v");
    assert_eq!(column.data_type, "integer");
    assert_eq!(
        column.comment.as_deref(),
        Some("a note"),
        "a catalog comment survives ALTER COLUMN TYPE verbatim - measured on \
         PostgreSQL 18.4 across every ALTER in the facet matrix"
    );
}

// ---------------------------------------------------------------------------
// REFUSED. Neither keeping nor clearing is truthful, so the fold fails closed -
// the treatment `setColumnType` already gives an encrypted column.
// ---------------------------------------------------------------------------

#[test]
fn a_retype_off_a_value_format_column_is_refused_rather_than_folded() {
    for source in [TYPE_ID, ULID] {
        for target in [TO_INT, TO_STRING_40, r#""text""#] {
            let error =
                pg_snapshot(source, target).expect_err("a value-format retype must fail closed");
            assert!(
                error.contains("value format"),
                "the refusal must name the facet it is refusing, got: {error}"
            );
        }
    }
}

#[test]
fn the_value_format_refusal_reaches_both_artifact_replays() {
    // the fold runs the catalog replay FIRST as its fail-closed structural
    // oracle, so one refusal covers `schema.runtime.json` and `env.db.ts` too.
    let ir = envelope(TYPE_ID, TO_INT);
    let effective = support::operator_charter(SCHEMA);
    let error = single_fold::fold(&ir.ops, SqlDialect::Postgres, SCHEMA, &effective)
        .map(|folded| folded.project_field_defs())
        .expect_err("the descriptor fold inherits the refusal");
    assert!(error.to_string().contains("value format"), "{error}");

    let error = zero_migrate::render::gen_types::render_artifacts(
        &ir.ops,
        SqlDialect::Postgres,
        SCHEMA,
        &effective,
    )
    .expect_err("artifact rendering inherits the refusal");
    assert!(error.to_string().contains("value format"), "{error}");
}

#[test]
fn a_retype_that_touches_no_value_format_column_is_untouched_by_the_refusal() {
    // The refusal keys on THE COLUMN BEING RETYPED, not on the table carrying a
    // value-format column somewhere. Without this, one typed-id column would
    // freeze every other column in its table.
    let ir: MigrationIr = serde_json::from_str(
        r#"{"ir_version":1,"name":"n","ops":[
            {"op":"createTable","name":"a","columns":[
                {"name":"c0","type":"int","nullable":false},
                {"name":"tid","type":"text","valueFormat":{"typeId":{"prefix":"usr"}}},
                {"name":"v","type":{"string":{"length":24}}}
            ],"primaryKey":["c0"]},
            {"op":"setColumnType","table":"a","column":"v","toType":"int"}
        ]}"#,
    )
    .expect("the envelope parses");
    let effective = support::operator_charter(SCHEMA);
    let folded = fold_ops(&ir.ops, SqlDialect::Postgres, SCHEMA, &effective)
        .expect("retyping a plain sibling is not refused");
    let table = folded.tables.get("a").expect("table a");
    assert_eq!(
        table
            .columns
            .iter()
            .find(|c| c.name == "v")
            .map(|c| c.data_type.as_str()),
        Some("integer"),
    );
    assert_eq!(
        table
            .columns
            .iter()
            .find(|c| c.name == "tid")
            .and_then(|c| c.value_format.clone()),
        Some(zero_migrate::model::ir::ValueFormat::TypeId {
            prefix: "usr".to_string()
        }),
        "the untouched typed-id column keeps its format"
    );
}
