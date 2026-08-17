//! An aggregate in a DML value position is refused at validate, not by the server.
//!
//! Every DML slot below is a SCALAR context, and PostgreSQL refuses an aggregate in
//! each of them. Measured on 18.4:
//!
//! ```text
//! UPDATE t SET n = count(n)              aggregate functions are not allowed in UPDATE
//! UPDATE t SET n = 1 WHERE count(n) > 0  aggregate functions are not allowed in WHERE
//! DELETE FROM t WHERE count(n) > 0       aggregate functions are not allowed in WHERE
//! INSERT INTO t VALUES (1, count(1))     aggregate functions are not allowed in VALUES
//! ```
//!
//! `validate_no_aggregate_expr_context` already existed and already carried the right
//! message; it guarded column defaults and never DML. So an authored `.count()` in an
//! assignment cleared validate AND preview, and the refusal arrived from the server
//! partway through a deploy - the fail-late shape this engine closes everywhere else.
//!
//! The arms are offline on purpose. The rule is not "PostgreSQL dislikes this", it is
//! that an aggregate has no meaning without a grouping context and a DML statement
//! has nowhere to put one, so it holds on every dialect and needs no database.

use zero_migrate::model::expr::Expr;
use zero_migrate::model::ir::{IrValue, MigrationIr, Op, CURRENT_IR_VERSION};
use zero_migrate::model::support::Dialect;
use zero_migrate::model::validate::validate_ir_scoped;
use zero_migrate::SchemaScope;

/// `count(<column>)` - the aggregate an author reaches for by accident.
fn count_of(column: &str) -> Expr {
    Expr::Agg {
        func: zero_migrate::model::expr::AggFunc::Count,
        arg: Some(Box::new(Expr::col(column))),
        delimiter: None,
        distinct: false,
    }
}

fn refusal_for(op: Op) -> Option<String> {
    let ir = MigrationIr {
        inverse_ops: None,
        irreversible: None,
        ir_version: CURRENT_IR_VERSION,
        name: "dml_aggregate".to_string(),
        owner_app: "app_agg".to_string(),
        ops: vec![op],
        flags: Default::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    };
    validate_ir_scoped(&ir, Dialect::Postgres, Some(&SchemaScope::Unconfined))
        .err()
        .map(|error| format!("{} {}", error.code, error.reason))
}

fn update_setting(value: Expr) -> Op {
    Op::Update {
        table: "t".to_string(),
        set: [("n".to_string(), IrValue::Expr(value))]
            .into_iter()
            .collect(),
        r#where: None,
        schema: None,
    }
}

#[test]
fn an_aggregate_in_an_update_assignment_is_refused() {
    let refusal = refusal_for(update_setting(count_of("n")))
        .expect("an aggregate assignment must be refused at validate");
    assert!(
        refusal.contains("AGGREGATE_IN_SCALAR_CONTEXT"),
        "the refusal must name the rule, got {refusal:?}"
    );
}

#[test]
fn an_aggregate_in_a_dml_predicate_is_refused_in_both_update_and_delete() {
    let update = Op::Update {
        table: "t".to_string(),
        set: [(
            "n".to_string(),
            IrValue::Expr(Expr::lit(zero_migrate::model::ir::IrScalar::Int(1))),
        )]
        .into_iter()
        .collect(),
        r#where: Some(Expr::BinOp {
            op: zero_migrate::model::expr::BinaryOp::Gt,
            lhs: Box::new(count_of("n")),
            rhs: Box::new(Expr::lit(zero_migrate::model::ir::IrScalar::Int(0))),
        }),
        schema: None,
    };
    let refusal =
        refusal_for(update).expect("an aggregate UPDATE predicate must be refused at validate");
    assert!(
        refusal.contains("AGGREGATE_IN_SCALAR_CONTEXT"),
        "update predicate refusal must name the rule, got {refusal:?}"
    );

    let delete = Op::Delete {
        table: "t".to_string(),
        r#where: Expr::BinOp {
            op: zero_migrate::model::expr::BinaryOp::Gt,
            lhs: Box::new(count_of("n")),
            rhs: Box::new(Expr::lit(zero_migrate::model::ir::IrScalar::Int(0))),
        },
        limit: None,
        schema: None,
    };
    let refusal =
        refusal_for(delete).expect("an aggregate DELETE predicate must be refused at validate");
    assert!(
        refusal.contains("AGGREGATE_IN_SCALAR_CONTEXT"),
        "delete predicate refusal must name the rule, got {refusal:?}"
    );
}

#[test]
fn a_plain_dml_value_still_validates() {
    // The control. Without it every assertion above would pass on a validator that
    // refused all DML for some unrelated reason - and the rule under test would be
    // measuring nothing, which is the failure this suite has hit more than once.
    assert_eq!(
        refusal_for(update_setting(Expr::lit(
            zero_migrate::model::ir::IrScalar::Int(1)
        ))),
        None,
        "an ordinary literal assignment must still clear validate"
    );
}
