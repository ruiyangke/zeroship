//! The offline op fold must replay the ops that change a column's shape.
//!
//! `fold_to_field_defs` reconstructs a per-table FieldDescriptor map by replaying
//! an envelope's ops. It handles eight of them - createTable, addColumn,
//! dropColumn, renameColumn, dropTable, renameTable, addConstraint,
//! alterPrimaryKey - and its catch-all drops the rest. The ops that change a
//! column's TYPE or its NULLABILITY are among the rest, so they were applied,
//! accepted, and then ignored by the reconstruction.
//!
//! WHY THIS IS NOT A COHERENCE BUG, which matters for where the fix belongs:
//! `fold_to_field_defs` calls `fold_ops` FIRST, and that is the fail-closed
//! structural oracle (add-to-missing-table, drop-absent-column, duplicate-create).
//! Coherence is already enforced. What leaks is FIDELITY - the op is legal and the
//! reconstruction simply does not reflect it.
//!
//! THE CONSUMER IS CODEGEN, which is what makes it user-facing rather than
//! internal. The IR schema records it directly: "the OFFLINE op fold
//! (`zero_migrate::fold_to_field_defs`) and `gen-types` have NO live DB". So a
//! migration that widens a column to `bigInt`, or tightens one to NOT NULL,
//! produces generated TypeScript that still describes the old shape - a
//! type-safety claim the codegen makes and the schema no longer honours.
//!
//! MEASURED BEFORE THIS FIXTURE EXISTED, by folding each envelope and printing the
//! result:
//!
//!     createTable a(v int); setColumnType a.v -> bigInt
//!         folded v = {"type":"int"}            STALE
//!     createTable a(v int NULL); setColumnNotNull a.v
//!         folded v = {"type":"int"}            STALE (no `required`)
//!     createTable a(c0 int NOT NULL); dropColumnNotNull a.c0
//!         folded c0 = {"required":true}        STALE (still required)
//!
//! SCOPE, stated rather than implied: three facet ops are pinned here because
//! three were measured. `setColumnDefault`, `dropColumnDefault`,
//! `synchronizeIdentity` and `dropConstraint` are also absent from the replay and
//! are NOT asserted, because my probes of them were inconclusive - one never
//! parsed, and one tested dropping a default from a column that had none, which a
//! correct implementation also leaves unchanged. They are covered instead by the
//! exhaustive match the fix installs: the compiler now demands a decision for
//! every op, so their status is recorded in an arm rather than guessed at here.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::render::fold::fold_to_field_defs;
use zero_migrate::schema::query::SqlDialect;

/// The folded FieldDescriptor map for table `a`, as JSON.
fn folded(ops_after_create: &str) -> serde_json::Value {
    let bytes = format!(
        r#"{{"ir_version":1,"name":"n","ops":[{{"op":"createTable","name":"a","columns":[{{"name":"c0","type":"int","nullable":false}},{{"name":"v","type":"int","nullable":true}}],"primaryKey":["c0"]}}{ops_after_create}]}}"#
    );
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    let effective = support::operator_charter("public");
    let map = fold_to_field_defs(&ir.ops, SqlDialect::Postgres, "public", &effective)
        .expect("the fold succeeds");
    map.get("a").cloned().expect("table a is in the fold")
}

fn field<'a>(table: &'a serde_json::Value, column: &str) -> &'a serde_json::Value {
    table
        .get(column)
        .expect("the column is in the folded table")
}

#[test]
fn the_baseline_shape_is_what_the_create_table_declared() {
    // Not decoration: every assertion below is a DIFFERENCE from this, so if the
    // baseline itself drifted the other tests would be measuring nothing.
    let a = folded("");
    assert_eq!(
        field(&a, "v").get("type").and_then(|t| t.as_str()),
        Some("int")
    );
    assert_eq!(
        field(&a, "v").get("required"),
        None,
        "v was declared nullable"
    );
    assert_eq!(
        field(&a, "c0")
            .get("required")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "c0 was declared NOT NULL"
    );
}

#[test]
fn set_column_type_is_replayed() {
    let a = folded(r#",{"op":"setColumnType","table":"a","column":"v","toType":"bigInt"}"#);
    assert_eq!(
        field(&a, "v").get("type").and_then(|t| t.as_str()),
        Some("bigInt"),
        "the fold must reflect the new column type, or gen-types emits the old one"
    );
}

#[test]
fn set_column_not_null_is_replayed() {
    let a = folded(r#",{"op":"setColumnNotNull","table":"a","column":"v"}"#);
    assert_eq!(
        field(&a, "v")
            .get("required")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "tightening a column to NOT NULL must make the generated field required"
    );
}

#[test]
fn drop_column_not_null_is_replayed() {
    // The other direction, and the one a fix written only into the tightening arm
    // would miss.
    let a = folded(r#",{"op":"dropColumnNotNull","table":"a","column":"c0"}"#);
    assert_eq!(
        field(&a, "c0").get("required"),
        None,
        "relaxing a column to NULL must make the generated field optional again"
    );
}
