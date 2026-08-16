//! A column named inside an EXPRESSION that an earlier `dropColumn` removed is
//! refused.
//!
//! The last open route of the use-after-drop family. The identifier-list routes
//! are closed by `op_after_rename_targets_old_name.rs` (tables),
//! `op_references_a_dropped_column.rs` (plain column references) and
//! `op_references_a_dropped_named_object.rs` (enums, domains, sequences, views).
//! Those fixtures each recorded expressions as deliberately out of reach.
//!
//! MEASURED FIRST, and both routes lowered:
//!
//!     ALTER TABLE "public"."a" DROP COLUMN "v"
//!     ALTER TABLE "public"."a" ADD CONSTRAINT "ck" CHECK (("v" > 0))
//!
//!     ALTER TABLE "public"."a" DROP COLUMN "v"
//!     ALTER TABLE "public"."a" ADD COLUMN "g" integer
//!         GENERATED ALWAYS AS (("v" + 1)) STORED
//!
//! Both confirmed against live PostgreSQL, which rejects each with
//! `ERROR: column "v" does not exist`.
//!
//! WHY THIS TURNED OUT TO BE CHEAP, when the earlier fixtures called it a
//! separate job: `Expr` is a CLOSED AST, not raw SQL, and the renderer already
//! carries an exhaustive walk over it — `render::dml::expr_column_refs`, whose
//! match has no catch-all arm. Nothing needed parsing and nothing needed a new
//! traversal; only the wiring was missing. The earlier note assumed expressions
//! meant text.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

const TABLE: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true},{"name":"w","type":"int","nullable":true}],"primaryKey":["c0"]}"#;

fn verdict(tail: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{TABLE},{tail}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, Dialect::Postgres, &[]).map_err(|e| format!("{}: {}", e.code, e.reason))
}

const DROP_V: &str = r#"{"op":"dropColumn","table":"a","column":"v"}"#;

/// Assert the op that named the column, so the CHECK-body sites and the
/// generated-column site cannot cover for each other.
///
/// Two of the three here are addConstraint and one is addColumn; all three name
/// the same column on the same table, so the op is the only discriminator the
/// message carries - and it carries it only because this rule was taught to name
/// its operation while this audit was running.
fn expect_expr_refusal(ops: &str, op_name: &str, what: &str) {
    let refusal = verdict(ops).expect_err(what);
    let expected = format!("this {op_name} names column");
    assert!(
        refusal.contains(&expected),
        "{expected:?} is missing, so a sibling op is satisfying this test: {refusal}"
    );
}

#[test]
fn a_check_body_naming_a_dropped_column_is_refused() {
    let refusal = verdict(&format!(
        r#"{DROP_V},{{"op":"addConstraint","table":"a","constraint":{{"name":"ck","kind":{{"kind":"check","expr":{{"node":"binOp","op":"gt","lhs":{{"node":"colRef","name":"v"}},"rhs":{{"node":"literal","value":0}}}}}}}}}}"#
    ))
    .expect_err("the CHECK body names a column the drop removed");
    assert!(
        refusal.to_lowercase().contains("drop"),
        "the refusal must point at the drop that removed it: {refusal}"
    );
}

#[test]
fn a_generated_column_naming_a_dropped_column_is_refused() {
    expect_expr_refusal(
        &format!(
            r#"{DROP_V},{{"op":"addColumn","table":"a","column":"g","type":"int","nullable":true,"generated":{{"expr":{{"node":"binOp","op":"add","lhs":{{"node":"colRef","name":"v"}},"rhs":{{"node":"literal","value":1}}}},"stored":true}}}}"#
        ),
        "addColumn",
        "the generated expression names a column the drop removed",
    );
}

#[test]
fn a_column_buried_deep_in_the_expression_is_still_found() {
    // Not a duplicate of the first test: it pins that the walk RECURSES rather
    // than peeking at the top node. A check that only looked at the outermost
    // node would pass the first test and miss this.
    expect_expr_refusal(
        &format!(
            r#"{DROP_V},{{"op":"addConstraint","table":"a","constraint":{{"name":"ck","kind":{{"kind":"check","expr":{{"node":"binOp","op":"or","lhs":{{"node":"binOp","op":"gt","lhs":{{"node":"colRef","name":"c0"}},"rhs":{{"node":"literal","value":0}}}},"rhs":{{"node":"case","branches":[{{"when":{{"node":"binOp","op":"gt","lhs":{{"node":"colRef","name":"v"}},"rhs":{{"node":"literal","value":0}}}},"then":{{"node":"literal","value":true}}}}],"else":{{"node":"literal","value":false}}}}}}}}}}}}"#
        ),
        "addConstraint",
        "a column reference nested inside CASE inside OR must still be found",
    );
}

// ---------------------------------------------------------------------------
// The controls. Each of these would pass if the fix were a blanket refusal.
// ---------------------------------------------------------------------------

#[test]
fn a_check_over_a_live_column_is_still_allowed() {
    verdict(
        r#"{"op":"addConstraint","table":"a","constraint":{"name":"ck","kind":{"kind":"check","expr":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"v"},"rhs":{"node":"literal","value":0}}}}}"#,
    )
    .expect("a CHECK over a column that exists is ordinary and must pass");
}

#[test]
fn a_check_naming_a_different_column_than_the_dropped_one_is_allowed() {
    verdict(&format!(
        r#"{DROP_V},{{"op":"addConstraint","table":"a","constraint":{{"name":"ck","kind":{{"kind":"check","expr":{{"node":"binOp","op":"gt","lhs":{{"node":"colRef","name":"w"}},"rhs":{{"node":"literal","value":0}}}}}}}}}}"#
    ))
    .expect("dropping one column must not block a CHECK over another");
}

#[test]
fn dropping_a_column_and_re_adding_it_before_the_check_is_still_allowed() {
    // The restoring move, which every member of this family has needed.
    verdict(&format!(
        r#"{DROP_V},{{"op":"addColumn","table":"a","column":"v","type":"int","nullable":true}},{{"op":"addConstraint","table":"a","constraint":{{"name":"ck","kind":{{"kind":"check","expr":{{"node":"binOp","op":"gt","lhs":{{"node":"colRef","name":"v"}},"rhs":{{"node":"literal","value":0}}}}}}}}}}"#
    ))
    .expect("drop, re-add, then constrain is a real migration pattern");
}

#[test]
fn a_check_on_another_table_is_not_affected_by_this_drop() {
    // The vacated set stays per TABLE, as the plain-reference walk already had it.
    let bytes = format!(
        r#"{{"ir_version":1,"name":"n","ops":[{TABLE},{{"op":"createTable","name":"b","columns":[{{"name":"c0","type":"int","nullable":false}},{{"name":"v","type":"int","nullable":true}}],"primaryKey":["c0"]}},{DROP_V},{{"op":"addConstraint","table":"b","constraint":{{"name":"ck","kind":{{"kind":"check","expr":{{"node":"binOp","op":"gt","lhs":{{"node":"colRef","name":"v"}},"rhs":{{"node":"literal","value":0}}}}}}}}}}]}}"#
    );
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, Dialect::Postgres, &[]).expect("dropping a.v must not block a CHECK over b.v");
}
