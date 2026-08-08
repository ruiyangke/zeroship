//! **`gen-types` cascades a column drop through the SELECTED dialect leg.**
//!
//! `render_expr_inline` installs exactly one leg of a `dialect({ pg, sqlite,
//! mysql })` expression per target, so the object that reaches the database names
//! only that leg's columns. The `gen-types` IR replay decides which constraints and
//! indexes survive an `Op::DropColumn`, and a `true` verdict DROPS - so a walk that
//! unions every leg is a FALSE POSITIVE: it removes an object the target kept.
//!
//! The history below lowers on PostgreSQL to
//! `CONSTRAINT "legs_leg_ck" CHECK (("a" > 0))` and
//! `CREATE INDEX "legs_partial_idx" ON ... ("id") WHERE ("a" > 0)` - neither names
//! `b`. Measured on PostgreSQL 18.4, `ALTER TABLE ... DROP COLUMN b` leaves both in
//! `pg_constraint` / `pg_indexes` untouched. `render/fold.rs` already models this:
//! its `DropColumn` CHECK cascade reads `render::dml::expr_column_refs`, which
//! descends only the selected leg. The `gen-types` replay is a SECOND replay of the
//! same history and has to agree, or the emitted artifact describes a table the
//! database does not have.
//!
//! No live database is needed: the defect is entirely in the offline replay, and
//! the arms below pin the selection rule from both sides.

mod support;

use zero_migrate::model::ir::{MigrationIr, Op};
use zero_migrate::{render_artifacts, SqlDialect};

const SCHEMA: &str = "public";

/// A `dialect()` predicate whose PostgreSQL leg reads `a` and whose SQLite leg
/// reads `b` - so the two targets disagree about which column drop cascades.
const SPLIT_PREDICATE: &str = r#"{
  "node":"dialect",
  "pg":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"a"},
        "rhs":{"node":"literal","value":0}},
  "sqlite":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"b"},
            "rhs":{"node":"literal","value":0}}
}"#;

/// The history: create `legs(id, a, b)` carrying the split predicate as a partial
/// index (and, on PostgreSQL, as a table CHECK too - a table-level CHECK is
/// PostgreSQL-only), then drop `dropped_column`.
fn history(dropped_column: &str, with_check: bool) -> Vec<Op> {
    let constraints = if with_check {
        format!(
            r#","constraints":[{{"name":"legs_leg_ck",
              "kind":{{"kind":"check","expr":{SPLIT_PREDICATE}}}}}]"#
        )
    } else {
        String::new()
    };
    let source = format!(
        r#"{{
  "ir_version": 1,
  "name": "dialectal_leg_cascade",
  "owner_app": "app_test",
  "ops": [
    {{"op":"createTable","name":"legs","columns":[
      {{"name":"id","type":"int","nullable":false}},
      {{"name":"a","type":"int","nullable":true}},
      {{"name":"b","type":"int","nullable":true}}
    ],"primaryKey":["id"]{constraints}}},
    {{"op":"createIndex","table":"legs","name":"legs_partial_idx",
     "columns":[{{"kind":"column","name":"id"}}],
     "where":{SPLIT_PREDICATE}}},
    {{"op":"dropColumn","table":"legs","column":"{dropped_column}"}}
  ]
}}"#
    );
    serde_json::from_str::<MigrationIr>(&source)
        .expect("the dialectal-leg test IR parses")
        .ops
}

fn env_db_ts(dropped_column: &str, dialect: SqlDialect, with_check: bool) -> String {
    render_artifacts(
        &history(dropped_column, with_check),
        dialect,
        SCHEMA,
        &support::no_inject(SCHEMA),
    )
    .expect("the dialectal-leg history renders artifacts")
    .env_db_ts
}

/// The reported defect. `b` appears ONLY in the inactive SQLite leg, so the
/// PostgreSQL objects never referenced it and PostgreSQL keeps both.
#[test]
fn dropping_a_column_named_only_by_an_inactive_leg_keeps_the_postgres_objects() {
    let generated = env_db_ts("b", SqlDialect::Postgres, true);
    assert!(
        generated.contains("legs_leg_ck"),
        "PostgreSQL renders CHECK ((\"a\" > 0)) and keeps it when `b` is dropped; the \
         artifact must keep it too: {generated}"
    );
    assert!(
        generated.contains("legs_partial_idx"),
        "PostgreSQL renders WHERE (\"a\" > 0) and keeps the partial index when `b` is \
         dropped; the artifact must keep it too: {generated}"
    );
}

/// The control on the same history: `a` IS what the PostgreSQL leg reads, so
/// PostgreSQL cascades both objects away and so must the replay. Without this arm
/// the fix above would also pass by never cascading a dialectal expression at all.
#[test]
fn dropping_the_column_the_selected_leg_reads_cascades_on_postgres() {
    let generated = env_db_ts("a", SqlDialect::Postgres, true);
    assert!(
        !generated.contains("legs_leg_ck"),
        "dropping the column the PostgreSQL leg reads cascades the CHECK: {generated}"
    );
    assert!(
        !generated.contains("legs_partial_idx"),
        "dropping the column the PostgreSQL leg reads cascades the partial index: \
         {generated}"
    );
}

/// The same partial index on the OTHER target, where the legs swap roles: SQLite
/// reads `b`, so dropping `b` cascades there and dropping `a` does not. This is
/// what makes the rule leg SELECTION rather than "a dialectal expression never
/// cascades". No CHECK here - a table-level CHECK is PostgreSQL-only.
#[test]
fn the_same_index_cascades_on_the_target_whose_leg_reads_the_column() {
    let dropped_b = env_db_ts("b", SqlDialect::Sqlite, false);
    assert!(
        !dropped_b.contains("legs_partial_idx"),
        "the SQLite leg reads `b`, so dropping `b` cascades the partial index there: \
         {dropped_b}"
    );
    let dropped_a = env_db_ts("a", SqlDialect::Sqlite, false);
    assert!(
        dropped_a.contains("legs_partial_idx"),
        "the SQLite leg never reads `a`, so dropping `a` leaves the partial index: \
         {dropped_a}"
    );
}
