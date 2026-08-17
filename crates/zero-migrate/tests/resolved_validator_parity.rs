//! The RESOLVED validator must refuse what the offline one refuses.
//!
//! `validate.rs` has two entry points. `validate_op` is offline and structural;
//! `validate_op_resolved` is the lower-time path that additionally resolves
//! `ColRef`s against the live target-table columns. Both validate DML.
//!
//! Three rules went into the offline arms first and reached the resolved ones late:
//! no aggregate in a value slot, no qualified reference to a second table, and no
//! volatile expression in a backfill filter. Every time, the suite stayed green,
//! because on the apply path the offline validator runs first and catches them.
//!
//! That is the problem this file exists for. A guarantee that holds only because two
//! validators run in a particular order is not a guarantee of either one, and the
//! next rule added to the offline arms will be just as invisible.
//!
//! So these call `validate_op_resolved` DIRECTLY, with a populated `live_columns`
//! map so the resolving branch is the one taken - the branch whose `else` falls back
//! to `validate_op` and would otherwise hide the gap.

use std::collections::BTreeMap;

use zero_migrate::model::expr::{AggFunc, BinaryOp, Expr};
use zero_migrate::model::ir::{BackfillSetValue, CursorStability, IrScalar, IrValue, Op};
use zero_migrate::model::support::Dialect;
use zero_migrate::model::validate::validate_op_resolved;

/// A live column map that RESOLVES `events`, so the resolving branch is taken.
fn live() -> BTreeMap<String, Vec<String>> {
    BTreeMap::from([(
        "events".to_string(),
        vec!["id".to_string(), "seen".to_string(), "tag".to_string()],
    )])
}

fn refusal(op: &Op) -> Option<String> {
    validate_op_resolved(op, Dialect::Postgres, &live(), 0)
        .err()
        .map(|error| format!("{} {}", error.code, error.reason))
}

fn count_of(column: &str) -> Expr {
    Expr::Agg {
        func: AggFunc::Count,
        arg: Some(Box::new(Expr::col(column))),
        delimiter: None,
        distinct: false,
    }
}

fn gt_zero(lhs: Expr) -> Expr {
    Expr::BinOp {
        op: BinaryOp::Gt,
        lhs: Box::new(lhs),
        rhs: Box::new(Expr::lit(IrScalar::Int(0))),
    }
}

fn update_where(predicate: Expr) -> Op {
    Op::Update {
        table: "events".to_string(),
        set: [(
            "tag".to_string(),
            IrValue::Expr(Expr::lit(IrScalar::Int(1))),
        )]
        .into_iter()
        .collect(),
        r#where: Some(predicate),
        schema: None,
    }
}

fn backfill_with(filter: Expr) -> Op {
    Op::Backfill {
        table: "events".to_string(),
        cursor_columns: vec!["id".to_string()],
        cursor_stability: CursorStability::ExternalInvariant {
            name: "events_frozen".to_string(),
        },
        set: [(
            "tag".to_string(),
            BackfillSetValue::Value(IrValue::Expr(Expr::lit(IrScalar::Int(1)))),
        )]
        .into_iter()
        .collect(),
        filter: Some(filter),
        batch_size: zero_migrate::model::ir::SafeU64::new(100).expect("a JS-safe batch size"),
        name: "backfill_parity".to_string(),
        schema: None,
    }
}

#[test]
fn the_resolved_path_refuses_an_aggregate_in_a_dml_predicate() {
    let refusal = refusal(&update_where(gt_zero(count_of("tag"))))
        .expect("the resolved validator must refuse an aggregate predicate on its own");
    assert!(
        refusal.contains("AGGREGATE_IN_SCALAR_CONTEXT"),
        "expected the aggregate rule, got {refusal:?}"
    );
}

#[test]
fn the_resolved_path_refuses_a_reference_to_a_second_table() {
    let foreign = gt_zero(Expr::ColRef {
        name: "ghost".to_string(),
        table: Some("other".to_string()),
    });
    let refusal = refusal(&update_where(foreign))
        .expect("the resolved validator must refuse a foreign qualifier on its own");
    assert!(
        refusal.contains("UNSUPPORTED") && refusal.contains("other"),
        "expected the qualifier rule naming the foreign table, got {refusal:?}"
    );
}

#[test]
fn the_resolved_path_refuses_a_volatile_backfill_filter() {
    // A backfill pages in batches, so a volatile filter selects a different cohort
    // each batch. This is the rule that reached the resolved arm last.
    let refusal = refusal(&backfill_with(Expr::BinOp {
        op: BinaryOp::Lt,
        lhs: Box::new(Expr::col("seen")),
        // `uuidV4` is the simplest node the volatility walker names; the rule is
        // about volatility, not about which volatile function it is.
        rhs: Box::new(Expr::UuidV4),
    }))
    .expect("the resolved validator must refuse a volatile backfill filter on its own");
    assert!(
        refusal.contains("immutable"),
        "expected the immutability rule, got {refusal:?}"
    );
}

#[test]
fn the_resolved_path_still_accepts_ordinary_dml() {
    // The control. Without it each assertion above would pass on a resolved validator
    // that refused everything - and this file would be measuring nothing, which is
    // the exact failure mode it was written to prevent elsewhere.
    assert_eq!(
        refusal(&update_where(gt_zero(Expr::col("tag")))),
        None,
        "an ordinary predicate over the target's own column must still validate"
    );
    assert_eq!(
        refusal(&backfill_with(gt_zero(Expr::col("tag")))),
        None,
        "an ordinary backfill filter must still validate"
    );
}
