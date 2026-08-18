//! A plain `replace` over a MATERIALIZED view is refused, because it would change the
//! object's KIND rather than its body.
//!
//! `materialized: true` together with `replace: true` is already refused upstream, at
//! `crates/zero-migrate/src/model/op_support.rs:240-243`. That is a DIFFERENT case and
//! checking it is what made this one easy to miss: the gap is a PLAIN replace -
//! `replace: true` with `materialized` absent or false - aimed at a view the folded
//! snapshot records as materialized.
//!
//! Until the fold read `replace` at all, the duplicate-name check refused this by
//! accident, with the wrong message but before anything ran. Teaching the fold to honour
//! `replace` removed that accidental refusal and let the projection overwrite
//! `materialized: true` with `false`, so the plan claimed a plain view where the database
//! holds a materialized one. PostgreSQL then rejects the `CREATE OR REPLACE VIEW`
//! mid-apply, which is a worse place to find out than planning.
//!
//! So the point of this arm is TIMING, not data safety: it moves the refusal back to
//! plan time where it was, without restoring the duplicate-name refusal that was wrong.
//!
//! Materialized views are PostgreSQL-only (`op_support.rs:246`), so no other dialect can
//! reach this shape.

use crate::support;

use zero_migrate::model::ir::ViewQuery;
use zero_migrate::{fold_ops, FoldError, Op, SqlDialect};

const SCHEMA: &str = "app";

fn create_view(name: &str, materialized: Option<bool>, replace: Option<bool>) -> Op {
    Op::CreateView {
        name: name.to_string(),
        schema: None,
        columns: None,
        query: ViewQuery::Raw {
            sql: "SELECT 1 AS n".to_string(),
        },
        replace,
        materialized,
    }
}

#[test]
fn a_plain_replace_over_a_materialized_view_is_refused_at_the_fold() {
    let effective = support::confined_charter();

    let error = fold_ops(
        &[
            create_view("totals", Some(true), None),
            create_view("totals", None, Some(true)),
        ],
        SqlDialect::Postgres,
        SCHEMA,
        &effective,
    )
    .expect_err("a replace may change a view's body, not whether it is materialized");

    // Asserted by variant rather than by message: `DuplicateView` would be the WRONG
    // refusal here even though it happens to be a refusal. The name does not collide -
    // re-declaring that name is exactly what `replace` licenses - so a test that
    // accepted any error would pass against the accidental refusal this replaced.
    assert!(
        matches!(&error, FoldError::ViewKindChanged { name, .. } if name == "totals"),
        "expected a kind-change refusal naming the view, got {error:?}",
    );
}

#[test]
fn a_plain_replace_over_a_plain_view_still_applies() {
    let effective = support::confined_charter();

    let folded = fold_ops(
        &[
            create_view("totals", None, None),
            create_view("totals", None, Some(true)),
        ],
        SqlDialect::Postgres,
        SCHEMA,
        &effective,
    )
    .expect("replacing a plain view with a plain view is the ordinary case");

    let view = folded.views.get("totals").expect("the view survives");
    assert!(!view.materialized, "the replaced view is still plain");
}

#[test]
fn a_materialized_replace_over_a_materialized_view_is_not_refused_by_this_check() {
    // Guards against writing the condition as "materialized must be false". The pair
    // materialized+replace is refused UPSTREAM at validation, so the fold must not be
    // the layer that decides it, and this arm fails if the kind check overreaches into
    // a case that is not a kind CHANGE.
    let effective = support::confined_charter();

    let folded = fold_ops(
        &[
            create_view("totals", Some(true), None),
            create_view("totals", Some(true), Some(true)),
        ],
        SqlDialect::Postgres,
        SCHEMA,
        &effective,
    )
    .expect("the kinds match, so this check has no opinion; validation owns the pair");

    let view = folded.views.get("totals").expect("the view survives");
    assert!(view.materialized, "the replaced view is still materialized");
}
