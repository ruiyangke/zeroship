//! Dropping a column an EXCLUDE constraint reads only through an EXPRESSION.
//!
//! The exclusion cascade set is built from PLAIN COLUMN elements only, mirroring
//! PostgreSQL's `conkey`, where an expression element contributes no plain column.
//! That much is right: PostgreSQL does NOT auto-cascade such a constraint.
//!
//! What neither side models is the other half. MEASURED against PostgreSQL 18.4, in
//! the same shape this test folds:
//!
//!     CREATE TABLE t (note text, other text,
//!       EXCLUDE USING btree (lower(note) WITH =));
//!     ALTER TABLE t DROP COLUMN note;
//!     ERROR:  cannot drop column note of table zm_low.t because other objects depend on it
//!     DETAIL:  constraint t_lower_excl on table zm_low.t depends on column note of table zm_low.t
//!
//! The same refusal was measured for a range expression
//! (`EXCLUDE USING gist (tstzrange(lo, hi) WITH &&)`, dropping `lo`), so it is the
//! expression rather than the particular function that carries the dependency.
//!
//! PostgreSQL REFUSES, through a dependency recorded against the expression rather
//! than through `conkey`. So the offline fold projects a schema the database will not
//! accept: the column gone, the constraint still standing.
//!
//! The contrast that makes the current cascade rule correct rather than lucky is the
//! plain-column form: `EXCLUDE USING gist (r WITH &&)` does NOT refuse - PostgreSQL
//! drops the constraint along with the column. So a cascade keyed on "this table has
//! an EXCLUDE" would reject migrations that apply cleanly today. Only the expression
//! form blocks, and only the expression form is projected wrongly.
//!
//! This pins the divergence rather than papering over it. The fix is a plan-time
//! dependency check shared with the rename case - a generated column, a view and an
//! expression-carrying exclusion all refuse the same way - and it belongs before
//! anything runs, not in the fold's cascade.

mod support;

use zero_migrate::{
    fold_ops, ColType, ColumnOrExpr, ExclusionElement, ExclusionMethod, ExclusionOperator, Expr,
    IrColumn, IrConstraint, IrConstraintKind, Op, ScalarFn, SqlDialect,
};

const SCHEMA: &str = "app";

fn col(name: &str, ty: ColType) -> IrColumn {
    IrColumn {
        name: name.to_string(),
        ty,
        nullable: Some(true),
        default: None,
        unique: None,
        value_format: None,
        references: None,
        id_prefix: None,
        collation: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
    }
}

/// `EXCLUDE USING btree (lower(note) WITH =)` - the column `note` is reachable only
/// by walking into the expression.
fn exclusion_over_an_expression() -> IrConstraint {
    IrConstraint {
        name: Some("stays_no_overlap".to_string()),
        kind: IrConstraintKind::Exclusion {
            using_method: ExclusionMethod::Gist,
            elements: vec![ExclusionElement {
                target: ColumnOrExpr::Expr {
                    expr: Expr::FnCall {
                        r#fn: ScalarFn::Lower,
                        args: vec![Expr::col("note")],
                    },
                },
                operator: ExclusionOperator::Equal,
            }],
            where_predicate: None,
            deferrable: None,
            initially_deferred: None,
        },
    }
}

fn create_stays(constraints: Vec<IrConstraint>) -> Op {
    Op::CreateTable {
        name: "stays".to_string(),
        columns: vec![
            col("lo", ColType::Timestamp),
            col("hi", ColType::Timestamp),
            col("note", ColType::Text),
        ],
        primary_key: None,
        constraints,
        indexes: Vec::new(),
        partition_by: None,
        runtime_options: None,
        schema: None,
        existence_guard: None,
    }
}

fn drop_column(column: &str) -> Op {
    Op::DropColumn {
        table: "stays".to_string(),
        column: column.to_string(),
        schema: None,
        existence_guard: None,
    }
}

#[test]
fn the_fold_projects_a_drop_postgres_refuses_when_an_exclusion_expression_reads_the_column() {
    let effective = support::confined_charter();

    let folded = fold_ops(
        &[
            create_stays(vec![exclusion_over_an_expression()]),
            drop_column("note"),
        ],
        SqlDialect::Postgres,
        SCHEMA,
        &effective,
    )
    .expect("the fold accepts the drop, which is the point of this test");

    let table = folded.tables.get("stays").expect("the table survives");
    assert!(
        !table.columns.iter().any(|c| c.name == "note"),
        "the fold removes the column: {:?}",
        table.columns.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
    assert!(
        table.constraints.iter().any(|c| c.kind == "EXCLUDE"),
        "and keeps the exclusion, because its cascade set holds plain columns only: {:?}",
        table
            .constraints
            .iter()
            .map(|c| &c.name)
            .collect::<Vec<_>>()
    );

    // So the projected table has an exclusion whose expression reads a column that
    // is no longer there. PostgreSQL refuses to produce that state at all; this is
    // the divergence, pinned until the plan-time dependency check lands.
}

// The CONTROL that keeps the rule above from being read too widely. A plain-column
// exclusion is auto-cascaded by PostgreSQL, and the fold cascades it identically, so
// nothing here should change for that shape.
#[test]
fn a_plain_column_exclusion_still_cascades_with_the_column_it_names() {
    let effective = support::confined_charter();
    let plain = IrConstraint {
        name: Some("stays_no_overlap_plain".to_string()),
        kind: IrConstraintKind::Exclusion {
            using_method: ExclusionMethod::Gist,
            elements: vec![ExclusionElement {
                target: ColumnOrExpr::Column {
                    name: "lo".to_string(),
                },
                operator: ExclusionOperator::Equal,
            }],
            where_predicate: None,
            deferrable: None,
            initially_deferred: None,
        },
    };

    let folded = fold_ops(
        &[create_stays(vec![plain]), drop_column("lo")],
        SqlDialect::Postgres,
        SCHEMA,
        &effective,
    )
    .expect("the fold accepts the drop");

    let table = folded.tables.get("stays").expect("the table survives");
    assert!(
        !table.constraints.iter().any(|c| c.kind == "EXCLUDE"),
        "a plain-column exclusion cascades away with its column, matching what \
         PostgreSQL does: {:?}",
        table
            .constraints
            .iter()
            .map(|c| &c.name)
            .collect::<Vec<_>>()
    );
}
