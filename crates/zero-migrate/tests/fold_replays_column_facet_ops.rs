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

// ---------------------------------------------------------------------------
// The DEFAULT facet. Measured after the three above were fixed, on the
// prediction that `FieldDescriptor.default` had the same shape of hole - the
// descriptor carries the slot, the ops write it, the replay ignored them.
//
// THESE NEED THEIR OWN BASELINE. An earlier probe tested "drop a default" on a
// column that had none, where "unchanged" is also the CORRECT answer, so it
// proved nothing. `folded_with_default` gives the column a default first, and
// the baseline test below asserts the fold can SEE it - without that, the drop
// test cannot distinguish a working implementation from a broken one.
// ---------------------------------------------------------------------------

/// Like [`folded`], but `v` is declared carrying a literal default.
fn folded_with_default(ops_after_create: &str) -> serde_json::Value {
    let bytes = format!(
        r#"{{"ir_version":1,"name":"n","ops":[{{"op":"createTable","name":"a","columns":[{{"name":"c0","type":"int","nullable":false}},{{"name":"v","type":"int","nullable":true,"default":{{"literal":{{"value":7}}}}}}],"primaryKey":["c0"]}}{ops_after_create}]}}"#
    );
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    let effective = support::operator_charter("public");
    let map = fold_to_field_defs(&ir.ops, SqlDialect::Postgres, "public", &effective)
        .expect("the fold succeeds");
    map.get("a").cloned().expect("table a is in the fold")
}

#[test]
fn a_declared_default_is_visible_to_the_fold() {
    // The precondition for the two tests below. If this ever stops holding, they
    // are measuring nothing and would pass for the wrong reason.
    let a = folded_with_default("");
    assert_eq!(
        field(&a, "v")
            .get("default")
            .and_then(serde_json::Value::as_i64),
        Some(7),
        "a default declared on createTable must reach the folded descriptor"
    );
}

#[test]
fn drop_column_default_is_replayed() {
    let a = folded_with_default(r#",{"op":"dropColumnDefault","table":"a","column":"v"}"#);
    assert_eq!(
        field(&a, "v").get("default"),
        None,
        "dropping the default must remove it from the generated field"
    );
}

#[test]
fn set_column_default_is_replayed() {
    let a = folded(
        r#",{"op":"setColumnDefault","table":"a","column":"v","value":{"literal":{"value":7}}}"#,
    );
    assert_eq!(
        field(&a, "v")
            .get("default")
            .and_then(serde_json::Value::as_i64),
        Some(7),
        "setting a default must reach the generated field"
    );
}

// ---------------------------------------------------------------------------
// The FK POLICY facet, which the fold LIFTS onto a ref-typed column.
//
// `addConstraint` was already replayed - it FEEDS the lift - while its inverse
// was not, so an ON DELETE outlived the constraint that granted it. That is
// worse than a stale type: it describes a DELETION BEHAVIOUR the database no
// longer has.
//
// BOTH AUTHORING ROUTES ARE PINNED. The policy can arrive inline on createTable
// or from a later addConstraint, and the fix had to record the constraint name
// at both push sites. A fixture testing one route would pass with the other half
// missing - the same inline-vs-standalone split that `f721_unguarded_index_shape`
// exists for.
// ---------------------------------------------------------------------------

const REF_TARGET: &str = r#"{"op":"createTable","name":"b","columns":[{"name":"c0","type":"int","nullable":false}],"primaryKey":["c0"]}"#;

fn folded_ref(a_table: &str, rest: &str) -> serde_json::Value {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{REF_TARGET},{a_table}{rest}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    let effective = support::operator_charter("public");
    let map = fold_to_field_defs(&ir.ops, SqlDialect::Postgres, "public", &effective)
        .expect("the fold succeeds");
    map.get("a").cloned().expect("table a is in the fold")
}

/// `a.v` is a ref to `b`, with the FK policy declared INLINE on the createTable.
const A_REF_INLINE_FK: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":{"ref":{"references":"b"}},"nullable":true}],"primaryKey":["c0"],"constraints":[{"name":"fk1","kind":{"kind":"fk","columns":["v"],"referencesTable":"b","referencesColumns":["c0"],"onDelete":"cascade"}}]}"#;
/// The same ref column with no constraint; the policy arrives via addConstraint.
const A_REF_ONLY: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":{"ref":{"references":"b"}},"nullable":true}],"primaryKey":["c0"]}"#;
const ADD_FK: &str = r#",{"op":"addConstraint","table":"a","constraint":{"name":"fk1","kind":{"kind":"fk","columns":["v"],"referencesTable":"b","referencesColumns":["c0"],"onDelete":"cascade"}}}"#;
const DROP_FK: &str = r#",{"op":"dropConstraint","table":"a","name":"fk1"}"#;

#[test]
fn an_inline_fk_policy_is_lifted_onto_the_ref_column() {
    // The precondition for the drop test below: without it, a drop test passes
    // trivially if the lift ever stops working.
    let a = folded_ref(A_REF_INLINE_FK, "");
    assert_eq!(
        field(&a, "v").get("onDelete").and_then(|v| v.as_str()),
        Some("cascade"),
        "the inline FK's policy must reach the ref column"
    );
}

#[test]
fn dropping_an_inline_declared_constraint_un_lifts_its_policy() {
    let a = folded_ref(A_REF_INLINE_FK, DROP_FK);
    assert_eq!(
        field(&a, "v").get("onDelete"),
        None,
        "the policy must not outlive the constraint that granted it"
    );
    assert_eq!(
        field(&a, "v").get("refTarget").and_then(|v| v.as_str()),
        Some("b"),
        "dropping the constraint removes the POLICY, not the reference itself"
    );
}

#[test]
fn dropping_an_add_constraint_declared_policy_un_lifts_it_too() {
    // The other authoring route. The fix records the constraint name at two push
    // sites; missing either leaves one route undroppable.
    let before = folded_ref(A_REF_ONLY, ADD_FK);
    assert_eq!(
        field(&before, "v").get("onDelete").and_then(|v| v.as_str()),
        Some("cascade"),
        "precondition: addConstraint's policy is lifted"
    );
    let after = folded_ref(A_REF_ONLY, &format!("{ADD_FK}{DROP_FK}"));
    assert_eq!(
        field(&after, "v").get("onDelete"),
        None,
        "the policy must not outlive the constraint that granted it"
    );
}
