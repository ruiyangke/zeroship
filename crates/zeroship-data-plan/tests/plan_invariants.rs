//! Invariants the node shapes cannot carry, and the refusals that do.
//!
//! SC-3 names three of these and is explicit that they need tests rather than
//! assumptions. Two are here (aggregate position; typed refusal from the
//! backend's own module); the third, parameter count equals placeholder
//! count, is in `tests/parameters_never_carry_values.rs`, because that is a
//! property of rendering.

use zeroship_data_plan::render::postgres::{self, RenderError};
use zeroship_data_plan::{
    AggregateFunc, AggregateRef, CompareOp, Direction, FieldPath, Ident, IdentRole, JsonKey,
    Literal, NullOrder, Operand, OrderKey, PlanError, Predicate, ProjectedField, Projection,
    RowLimit, RowOffset, Select, MAX_ROW_LIMIT, MAX_ROW_OFFSET,
};

fn column(name: &str) -> Ident {
    Ident::parse_as(name, IdentRole::Column).expect("valid column")
}

fn alias(name: &str) -> Ident {
    Ident::parse_as(name, IdentRole::Alias).expect("valid alias")
}

fn collection() -> Ident {
    Ident::parse_as("orders", IdentRole::Collection).expect("valid collection")
}

fn rows() -> Projection {
    Projection::rows(vec![
        ProjectedField::column(column("total")).expect("projectable")
    ])
    .expect("row projection")
}

fn count_projection() -> Projection {
    Projection::aggregate(vec![
        ProjectedField::aggregate(AggregateRef::count_rows(), alias("total_rows")),
        ProjectedField::column(column("status")).expect("projectable"),
    ])
    .expect("aggregate projection")
}

fn count_gt(n: i64) -> Predicate {
    Predicate::compare(
        Operand::Aggregate(AggregateRef::count_rows()),
        CompareOp::Gt,
        Operand::Lit(Literal::Int(n)),
    )
}

/// An aggregate operand is legal only in a HAVING position. In `WHERE` it is a
/// `PostgreSQL` error, and it is the mistake a builder makes when `HAVING` is
/// spelled as "another filter".
#[test]
fn an_aggregate_operand_in_a_filter_is_refused() {
    let outcome = Select::builder(collection(), rows())
        .filter(count_gt(5))
        .build();
    assert_eq!(
        outcome.expect_err("must refuse"),
        PlanError::AggregateOutsideHaving
    );
    println!("ruled on 1 misplaced aggregate");
}

/// The control for the arm above: the same operand in the position where it IS
/// legal must be accepted, or the refusal is a blanket ban and proves nothing
/// about position.
#[test]
fn the_same_aggregate_operand_is_accepted_in_having() {
    let plan = Select::builder(collection(), count_projection())
        .group_by(vec![FieldPath::column(column("status"))])
        .having(count_gt(5))
        .build()
        .expect("valid plan");
    let sql = postgres::render_select(&plan).expect("renders").sql().to_string();
    assert!(
        sql.contains(r#"GROUP BY "status""#) && sql.contains("HAVING COUNT(*) > $1"),
        "unexpected aggregate lowering: {sql}"
    );
    println!("ruled on 1 HAVING plan");
}

/// A grouped read that projects an ungrouped, unaggregated column is invalid
/// SQL. Refusing at plan construction turns a server error into a typed one
/// that names the offending alias.
#[test]
fn an_ungrouped_projected_column_is_refused() {
    let outcome = Select::builder(collection(), count_projection())
        .group_by(vec![FieldPath::column(column("region"))])
        .build();
    assert!(matches!(
        outcome,
        Err(PlanError::UngroupedProjectedField { .. })
    ));
    println!("ruled on 1 ungrouped projection");
}

/// GROUP BY over a row projection would group by the platform fields, which are
/// not grouping keys. The two projection kinds exist for exactly this.
#[test]
fn group_by_over_a_row_projection_is_refused() {
    let outcome = Select::builder(collection(), rows())
        .group_by(vec![FieldPath::column(column("status"))])
        .build();
    assert_eq!(
        outcome.expect_err("must refuse"),
        PlanError::GroupByWithRowProjection
    );
    println!("ruled on 1 grouped row projection");
}

/// HAVING without an aggregate projection is a filter wearing the wrong name.
#[test]
fn having_without_an_aggregate_projection_is_refused() {
    let outcome = Select::builder(collection(), rows())
        .having(Predicate::compare(
            Operand::column(column("total")),
            CompareOp::Gt,
            Operand::Lit(Literal::Int(1)),
        ))
        .build();
    assert_eq!(
        outcome.expect_err("must refuse"),
        PlanError::HavingWithoutAggregate
    );
    println!("ruled on 1 misplaced HAVING");
}

/// A read is never unbounded. An omitted limit defaults to the cap rather than
/// to no `LIMIT` clause, which is what once pulled an entire collection into
/// the worker.
#[test]
fn a_read_always_carries_a_bounded_limit() {
    let plan = Select::builder(collection(), rows())
        .build()
        .expect("valid plan");
    assert_eq!(plan.limit().get(), MAX_ROW_LIMIT);
    let rendered = postgres::render_select(&plan).expect("renders");
    assert!(
        rendered.sql().contains("LIMIT $"),
        "a read rendered without a LIMIT clause: {}",
        rendered.sql()
    );

    let mut ruled_on = 0_usize;
    for bad in [0_i64, -1, MAX_ROW_LIMIT + 1, i64::MAX] {
        assert_eq!(
            RowLimit::new(bad).expect_err("must refuse"),
            PlanError::LimitOutOfRange { requested: bad }
        );
        ruled_on += 1;
    }
    for good in [1_i64, MAX_ROW_LIMIT] {
        assert!(RowLimit::new(good).is_ok());
        ruled_on += 1;
    }
    assert_eq!(ruled_on, 6);
    assert!(RowOffset::new(0).is_ok());
    assert!(RowOffset::new(MAX_ROW_OFFSET).is_ok());
    assert!(RowOffset::new(MAX_ROW_OFFSET + 1).is_err());
    assert!(RowOffset::new(-1).is_err());
    println!("ruled on {ruled_on} limits and 4 offsets");
}

/// SC-3's decision 2: a backend that cannot serve a node refuses, with a typed
/// error from its own module. Not an empty result, and not an emulation.
///
/// The subject is a nested JSON path, whose shape SC-3 pins into the shared
/// grammar and whose lowering is deliberately unwritten.
#[test]
fn an_unservable_node_is_refused_with_a_typed_error() {
    let nested = FieldPath::nested(
        column("payload"),
        vec![JsonKey::new("customer").expect("key")],
    )
    .expect("nested path");
    let plan = Select::builder(collection(), rows())
        .filter(Predicate::compare(
            Operand::Path(nested),
            CompareOp::Eq,
            Operand::Lit(Literal::text("acme").expect("text")),
        ))
        .build()
        .expect("the PLAN is valid; only the lowering refuses");
    let outcome = postgres::render_select(&plan);
    assert!(
        matches!(outcome, Err(RenderError::Unsupported { .. })),
        "expected a typed refusal, got {outcome:?}"
    );
    let message = outcome.expect_err("refused").to_string();
    assert!(
        message.contains("postgres") && message.contains("nested"),
        "the refusal does not name the backend and the node: {message}"
    );
    println!("ruled on 1 unservable node");
}

/// The control for the arm above: the plan is refused by the LOWERING, not by
/// the grammar, so the same shape without nesting must render.
#[test]
fn the_same_shape_without_nesting_renders() {
    let plan = Select::builder(collection(), rows())
        .filter(Predicate::compare(
            Operand::column(column("payload")),
            CompareOp::Eq,
            Operand::Lit(Literal::text("acme").expect("text")),
        ))
        .build()
        .expect("valid plan");
    assert!(postgres::render_select(&plan).is_ok());
    println!("ruled on 1 flat path");
}

/// Nulls sort explicitly, because ``PostgreSQL`` and ``SQLite`` disagree by default
/// an implicit choice makes the dev tier and the production tier return
/// different pages for the same plan.
#[test]
fn null_ordering_is_always_explicit_in_the_statement() {
    let mut ruled_on = 0_usize;
    for (nulls, expected) in [
        (NullOrder::First, "NULLS FIRST"),
        (NullOrder::Last, "NULLS LAST"),
    ] {
        let plan = Select::builder(collection(), rows())
            .order_by(vec![OrderKey {
                path: FieldPath::column(column("total")),
                direction: Direction::Descending,
                nulls,
            }])
            .build()
            .expect("valid plan");
        let sql = postgres::render_select(&plan).expect("renders").sql().to_string();
        assert!(
            sql.contains(expected),
            "null ordering was left implicit: {sql}"
        );
        ruled_on += 1;
    }
    assert_eq!(ruled_on, 2);
    println!("ruled on {ruled_on} null orderings");
}

/// DISTINCT on an aggregate is a different thing from DISTINCT on a read, and
/// only COUNT gets it - `SUM(DISTINCT x)` is legal SQL and almost never what a
/// caller means.
#[test]
fn distinct_is_restricted_to_count() {
    assert!(AggregateRef::over(AggregateFunc::Count, column("id"), true).is_ok());
    assert!(AggregateRef::over(AggregateFunc::Sum, column("total"), true).is_err());
    assert!(AggregateRef::over(AggregateFunc::Sum, column("total"), false).is_ok());
    println!("ruled on 3 aggregate shapes");
}
