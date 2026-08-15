//! A DML statement may not reference another table through a qualified column.
//!
//! `UPDATE users SET n = 8 WHERE other.ghost > 0` used to clear validate and preview
//! and be refused by PostgreSQL partway through the deploy:
//!
//! ```text
//! SERVER_REJECTED sqlstate=42P01 missing FROM-clause entry for table "other"
//! ```
//!
//! The expression walker accepted any qualified `ColRef` structurally, under a
//! comment saying the real scope check "is coupled with the view/FROM builder". For a
//! view that is right - a SELECT may join, so a qualifier naming another table is
//! legal and only the FROM set can settle it. A DML statement has no FROM set to
//! defer to, so nothing ever checked it.
//!
//! It needs no catalog. A DML statement has exactly one target table, so the only
//! legal qualifier is that target - true offline, on every dialect, whether or not
//! the other table exists.
//!
//! THE CONTROL BELOW IS THE POINT. The fix works by making the strict mode opt-in
//! (`TargetScope::refusing_foreign_qualifiers`) precisely so views keep their lenient
//! pass. A joined view is the shape that would break if that opt-in were ever turned
//! into a default, and the third test here is what would notice.

use zero_migrate::model::expr::{BinaryOp, Expr};
use zero_migrate::model::ir::{IrScalar, IrValue, MigrationIr, Op, CURRENT_IR_VERSION};
use zero_migrate::model::support::Dialect;
use zero_migrate::model::validate::validate_ir_scoped;
use zero_migrate::SchemaScope;

fn refusal_for(op: Op) -> Option<String> {
    let ir = MigrationIr {
        inverse_ops: None,
        irreversible: None,
        ir_version: CURRENT_IR_VERSION,
        name: "dml_qualified_ref".to_string(),
        owner_app: "app_qref".to_string(),
        ops: vec![op],
        flags: Default::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    };
    validate_ir_scoped(&ir, Dialect::Postgres, &[], Some(&SchemaScope::Unconfined))
        .err()
        .map(|error| format!("{} {}", error.code, error.reason))
}

/// `<table>.<column> > 0`, the predicate shape the defect was measured with.
fn qualified_predicate(table: Option<&str>, column: &str) -> Expr {
    Expr::BinOp {
        op: BinaryOp::Gt,
        lhs: Box::new(Expr::ColRef {
            name: column.to_string(),
            table: table.map(ToString::to_string),
        }),
        rhs: Box::new(Expr::lit(IrScalar::Int(0))),
    }
}

fn update_on_users(predicate: Expr) -> Op {
    Op::Update {
        table: "users".to_string(),
        set: [("n".to_string(), IrValue::Expr(Expr::lit(IrScalar::Int(8))))]
            .into_iter()
            .collect(),
        r#where: Some(predicate),
        schema: None,
    }
}

#[test]
fn a_qualified_reference_to_another_table_is_refused_in_a_dml_predicate() {
    let refusal = refusal_for(update_on_users(qualified_predicate(Some("other"), "ghost")))
        .expect("a cross-table qualified reference must be refused at validate");
    assert!(
        refusal.contains("UNSUPPORTED"),
        "the refusal must be an UNSUPPORTED authoring error, got {refusal:?}"
    );
    assert!(
        refusal.contains("other") && refusal.contains("users"),
        "the refusal must name both the foreign qualifier and the target, got {refusal:?}"
    );
}

#[test]
fn the_targets_own_name_is_still_a_legal_qualifier_and_a_bare_column_still_is_too() {
    // `users.n` names the statement's own target: legal, and the rule must not
    // refuse it just because a qualifier is present. Without this the fix could be
    // "refuse every qualified ref" and still pass the arm above.
    assert_eq!(
        refusal_for(update_on_users(qualified_predicate(Some("users"), "n"))),
        None,
        "a qualifier naming the target table itself must still validate"
    );
    assert_eq!(
        refusal_for(update_on_users(qualified_predicate(None, "n"))),
        None,
        "an unqualified column must still validate"
    );
}

#[test]
fn a_view_that_joins_keeps_its_lenient_pass() {
    // The control this file exists for. A view's SELECT may join, so a qualified ref
    // naming the joined table is legal and only the FROM set can settle it. The DML
    // rule is opt-in for exactly this reason, and if it were ever made the default
    // this test is what would fail rather than a user's view silently breaking.
    let create_view: MigrationIr = serde_json::from_str(
        r#"{
          "ir_version": 1,
          "name": "joined_view",
          "owner_app": "app_qref",
          "ops": [
            {"op":"createView","name":"joined",
             "query":{"kind":"structured","select":{
               "from":{"name":"users"},
               "joins":[{"kind":"inner","table":{"name":"orders"},
                 "on":{"node":"binOp","op":"eq",
                       "lhs":{"node":"colRef","name":"id","table":"users"},
                       "rhs":{"node":"colRef","name":"user_id","table":"orders"}}}],
               "projection":[{"kind":"colRef","name":"id","table":"users"},
                             {"kind":"colRef","name":"user_id","table":"orders"}]}}}
          ]
        }"#,
    )
    .expect("the joined-view fixture parses");

    let outcome = validate_ir_scoped(
        &create_view,
        Dialect::Postgres,
        &[],
        Some(&SchemaScope::Unconfined),
    );
    assert!(
        outcome.is_ok(),
        "a view joining another table must still validate its qualified references; got {:?}",
        outcome.err().map(|e| e.reason)
    );
}
