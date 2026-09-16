//! The offline op fold must replay the ops that change a column's shape.
//!
//! The `FieldDef` projection reconstructs a per-table FieldDescriptor map from
//! an envelope's ops, and the ops that change a column's TYPE, its NULLABILITY,
//! its DEFAULT, its FK policy or its single-column uniqueness must be reflected
//! in that reconstruction - not applied, accepted, and ignored.
//!
//! WHY THIS IS NOT A COHERENCE QUESTION, which matters for where the check
//! belongs: the fold runs the catalog replay FIRST, and that is the fail-closed
//! structural oracle (add-to-missing-table, drop-absent-column, duplicate-create).
//! Coherence is enforced there. What can leak is FIDELITY - the op is legal and
//! the reconstruction must still reflect it.
//!
//! THE CONSUMER IS CODEGEN, which is what makes it user-facing rather than
//! internal. The IR schema records it directly: the OFFLINE op fold and
//! `gen-types` have NO live DB. So a migration that widens a column to `bigInt`,
//! or tightens one to NOT NULL, must change the generated TypeScript too -
//! otherwise a type-safety claim the codegen makes stops being true.
//!
//! SCOPE: `AuthoredState::advance` matches on every `Op` with no catch-all arm,
//! so the compiler demands a decision for each one. The arms below pin the
//! decisions that move a column facet; an op that legitimately leaves the
//! descriptor alone is recorded in an arm of that match rather than guessed at
//! here.

use crate::support;

use zeroship_migrate::model::ir::MigrationIr;
use zeroship_migrate::render::fold::single_fold;

/// The folded FieldDescriptor map for table `a`, as JSON.
fn folded(ops_after_create: &str) -> serde_json::Value {
    let bytes = format!(
        r#"{{"ir_version":1,"name":"n","ops":[{{"op":"createTable","name":"a","columns":[{{"name":"c0","type":"int","nullable":false}},{{"name":"v","type":"int","nullable":true}}],"primaryKey":["c0"]}}{ops_after_create}]}}"#
    );
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    let effective = support::operator_charter("public");
    let map = single_fold::fold(
        zeroship_migrate::shipping_vendors(),
        &ir.ops,
        &zeroship_migrate_postgres::DIALECT,
        "public",
        &effective,
    )
    .map(|folded| folded.project_field_defs(zeroship_migrate::shipping_vendors()))
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
    // The other direction - the one an implementation that only tightened would
    // miss.
    let a = folded(r#",{"op":"dropColumnNotNull","table":"a","column":"c0"}"#);
    assert_eq!(
        field(&a, "c0").get("required"),
        None,
        "relaxing a column to NULL must make the generated field optional again"
    );
}

// ---------------------------------------------------------------------------
// The DEFAULT facet. The descriptor carries the slot and the ops write it, so
// the replay must reflect them.
//
// THESE NEED THEIR OWN BASELINE. "Drop a default" on a column that has none is
// a case where "unchanged" is also the CORRECT answer, so it proves nothing.
// `folded_with_default` gives the column a default first, and the baseline test
// below asserts the fold can SEE it - without that, the drop test cannot
// distinguish a working implementation from a broken one.
// ---------------------------------------------------------------------------

/// Like [`folded`], but `v` is declared carrying a literal default.
fn folded_with_default(ops_after_create: &str) -> serde_json::Value {
    let bytes = format!(
        r#"{{"ir_version":1,"name":"n","ops":[{{"op":"createTable","name":"a","columns":[{{"name":"c0","type":"int","nullable":false}},{{"name":"v","type":"int","nullable":true,"default":{{"literal":{{"value":7}}}}}}],"primaryKey":["c0"]}}{ops_after_create}]}}"#
    );
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    let effective = support::operator_charter("public");
    let map = single_fold::fold(
        zeroship_migrate::shipping_vendors(),
        &ir.ops,
        &zeroship_migrate_postgres::DIALECT,
        "public",
        &effective,
    )
    .map(|folded| folded.project_field_defs(zeroship_migrate::shipping_vendors()))
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
// `addConstraint` is replayed - it FEEDS the lift - so its inverse must be
// replayed too, or an ON DELETE outlives the constraint that granted it. That
// is worse than a stale type: it describes a DELETION BEHAVIOUR the database no
// longer has.
//
// BOTH AUTHORING ROUTES ARE PINNED. The policy can arrive inline on createTable
// or from a later addConstraint, and the fold records the constraint name at
// both push sites. A fixture testing one route would pass with the other half
// missing - the same inline-vs-standalone split that `f721_unguarded_index_shape`
// exists for.
// ---------------------------------------------------------------------------

const REF_TARGET: &str = r#"{"op":"createTable","name":"b","columns":[{"name":"c0","type":"text","nullable":false}],"primaryKey":["c0"]}"#;

fn folded_ref(a_table: &str, rest: &str) -> serde_json::Value {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{REF_TARGET},{a_table}{rest}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    let effective = support::operator_charter("public");
    let map = single_fold::fold(
        zeroship_migrate::shipping_vendors(),
        &ir.ops,
        &zeroship_migrate_postgres::DIALECT,
        "public",
        &effective,
    )
    .map(|folded| folded.project_field_defs(zeroship_migrate::shipping_vendors()))
    .expect("the fold succeeds");
    map.get("a").cloned().expect("table a is in the fold")
}

/// `a.v` is a ref to `b`, with the FK policy declared INLINE on the createTable.
const A_REF_INLINE_FK: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":{"ref":{"references":"b"}},"references":{"table":"b","column":"c0"},"nullable":true}],"primaryKey":["c0"],"constraints":[{"name":"fk1","kind":{"kind":"fk","columns":["v"],"referencesTable":"b","referencesColumns":["c0"],"onDelete":"cascade"}}]}"#;
/// The same ref column with no constraint; the policy arrives via addConstraint.
const A_REF_ONLY: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":{"ref":{"references":"b"}},"references":{"table":"b","column":"c0"},"nullable":true}],"primaryKey":["c0"]}"#;
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
    // The other authoring route. The fold records the constraint name at two push
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

// ---------------------------------------------------------------------------
// A SINGLE-COLUMN UNIQUE is a column facet, whichever way it is authored.
//
// `t.string().unique()` sets `unique` on the descriptor; the same constraint
// declared at table level - inline on createTable, or via addConstraint - must
// reach the same field, or two authoring routes to one uniqueness produce
// different generated types.
//
// Multi-column unique is deliberately NOT a column facet: it is a table key,
// and the fold already draws that single-column line for foreign-key policy.
// ---------------------------------------------------------------------------

fn folded_table(a_table: &str, rest: &str) -> serde_json::Value {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{a_table}{rest}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    let effective = support::operator_charter("public");
    let map = single_fold::fold(
        zeroship_migrate::shipping_vendors(),
        &ir.ops,
        &zeroship_migrate_postgres::DIALECT,
        "public",
        &effective,
    )
    .map(|folded| folded.project_field_defs(zeroship_migrate::shipping_vendors()))
    .expect("the fold succeeds");
    map.get("a").cloned().expect("table a is in the fold")
}

const A_COL_UNIQUE: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true,"unique":true}],"primaryKey":["c0"]}"#;
const A_TBL_UNIQUE: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true}],"primaryKey":["c0"],"constraints":[{"name":"u1","kind":{"kind":"unique","columns":["v"]}}]}"#;
const A_PLAIN: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true}],"primaryKey":["c0"]}"#;
const ADD_UNIQUE: &str = r#",{"op":"addConstraint","table":"a","constraint":{"name":"u1","kind":{"kind":"unique","columns":["v"]}}}"#;

#[test]
fn a_column_level_unique_is_visible_to_the_fold() {
    // The precondition: without it, the two tests below could pass by the
    // descriptor's `unique` slot being unreachable rather than by the lift.
    let a = folded_table(A_COL_UNIQUE, "");
    assert_eq!(
        field(&a, "v")
            .get("unique")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "a column-level unique must reach the folded descriptor"
    );
}

#[test]
fn an_inline_table_level_unique_reaches_the_column() {
    let a = folded_table(A_TBL_UNIQUE, "");
    assert_eq!(
        field(&a, "v")
            .get("unique")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "the same uniqueness authored at table level must reach the same field"
    );
}

#[test]
fn an_add_constraint_unique_reaches_the_column() {
    let a = folded_table(A_PLAIN, ADD_UNIQUE);
    assert_eq!(
        field(&a, "v")
            .get("unique")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "the addConstraint route must agree with the inline one"
    );
}

#[test]
fn a_multi_column_unique_is_not_a_column_facet() {
    // THE BOUNDARY. A composite unique is a table key; marking either member
    // column `unique` would claim each is independently unique, which is false.
    let ops = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true}],"primaryKey":["c0"],"constraints":[{"name":"u2","kind":{"kind":"unique","columns":["c0","v"]}}]}"#;
    let a = folded_table(ops, "");
    assert_eq!(
        field(&a, "v").get("unique"),
        None,
        "a composite unique must not mark its members individually unique"
    );
}
