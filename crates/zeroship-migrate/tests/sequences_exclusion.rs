use std::collections::BTreeSet;

use serde_json::json;
use zeroship_migrate::model::ir::ExistenceGuard;
use zeroship_migrate::render::lower::{IrAuthor, IrLowerError};
use zeroship_migrate::{
    fold_ops, BinaryOp, ColType, ColumnOrExpr, ExclusionElement, ExclusionMethod,
    ExclusionOperator, Expr, IrConstraint, IrConstraintKind, IrScalar, LiveSchema, MigrationIr,
    Op, ScalarFn, SequenceOwnedBy,
};
use zeroship_schema::query::SqlDialect;

const SCHEMA: &str = "app";
const OWNER: &str = "app_test";

fn ir(ops: Vec<Op>) -> MigrationIr {
    MigrationIr {
        ir_version: 1,
        name: "m".into(),
        owner_app: OWNER.into(),
        ops,
        flags: Default::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    }
}

fn lower(ops: Vec<Op>, dialect: SqlDialect, live: &BTreeSet<String>) -> Vec<zeroship_migrate::Migration> {
    let author = IrAuthor::new(SCHEMA, OWNER, dialect);
    author
        .lower(&ir(ops), &LiveSchema::from(live))
        .expect("lower")
}

fn create_sequence_op() -> Op {
    Op::CreateSequence {
        name: "invoice_seq".into(),
        schema: None,
        as_type: Some(ColType::BigInt),
        increment: Some(5),
        start: Some(100),
        min_value: None,
        max_value: None,
        cache: Some(10),
        cycle: Some(true),
        owned_by: Some(Some(SequenceOwnedBy {
            table: "invoices".into(),
            column: "id".into(),
        })),
    }
}

#[test]
fn postgres_renders_create_alter_drop_sequence() {
    let create = lower(vec![create_sequence_op()], SqlDialect::Postgres, &BTreeSet::new());
    assert_eq!(
        create[0].up,
        r#"CREATE SEQUENCE "app"."invoice_seq" AS bigint INCREMENT BY 5 START WITH 100 CACHE 10 CYCLE OWNED BY "app"."invoices"."id""#
    );
    assert_eq!(
        create[0].down.as_deref(),
        Some(r#"DROP SEQUENCE "app"."invoice_seq""#)
    );

    let alter = lower(
        vec![Op::AlterSequence {
            name: "invoice_seq".into(),
            schema: None,
            increment: Some(7),
            restart: Some(Some(200)),
            min_value: Some(Some(1)),
            max_value: Some(Some(999)),
            cache: Some(20),
            cycle: Some(false),
            owned_by: Some(None),
        }],
        SqlDialect::Postgres,
        &BTreeSet::new(),
    );
    assert_eq!(
        alter[0].up,
        r#"ALTER SEQUENCE "app"."invoice_seq" INCREMENT BY 7 RESTART WITH 200 MINVALUE 1 MAXVALUE 999 CACHE 20 NO CYCLE OWNED BY NONE"#
    );
    assert_eq!(alter[0].down, None);

    let drop = lower(
        vec![Op::DropSequence {
            name: "invoice_seq".into(),
            schema: None,
            existence_guard: Some(ExistenceGuard::IfExists),
        }],
        SqlDialect::Postgres,
        &BTreeSet::new(),
    );
    assert_eq!(drop[0].up, r#"DROP SEQUENCE IF EXISTS "app"."invoice_seq""#);
}

#[test]
fn sqlite_and_mysql_fail_closed_on_sequences() {
    for dialect in [SqlDialect::Sqlite, SqlDialect::Mysql] {
        let err = IrAuthor::new(SCHEMA, OWNER, dialect)
            .lower(&ir(vec![create_sequence_op()]), &LiveSchema::default())
            .unwrap_err();
        assert!(matches!(
            err,
            IrLowerError::SequenceUnsupported { kind: "sequence", dialect: d } if d == dialect
        ));
    }
}

#[test]
fn fold_tracks_sequence_existence_and_drop() {
    let created = fold_ops(&[create_sequence_op()], SqlDialect::Postgres, SCHEMA).expect("fold create");
    assert!(created.sequences.contains_key("invoice_seq"));

    let dropped = fold_ops(
        &[
            create_sequence_op(),
            Op::DropSequence {
                name: "invoice_seq".into(),
                schema: None,
                existence_guard: None,
            },
        ],
        SqlDialect::Postgres,
        SCHEMA,
    )
    .expect("fold drop");
    assert!(!dropped.sequences.contains_key("invoice_seq"));
}

fn exclusion_constraint() -> IrConstraint {
    IrConstraint {
        name: Some("bookings_no_overlap".into()),
        kind: IrConstraintKind::Exclusion {
            using_method: ExclusionMethod::Gist,
            elements: vec![
                ExclusionElement {
                    target: ColumnOrExpr::Column { name: "room".into() },
                    operator: ExclusionOperator::Equal,
                },
                ExclusionElement {
                    target: ColumnOrExpr::Column { name: "during".into() },
                    operator: ExclusionOperator::Overlaps,
                },
            ],
            where_predicate: Some(Expr::BinOp {
                op: BinaryOp::Eq,
                lhs: Box::new(Expr::col("cancelled")),
                rhs: Box::new(Expr::lit(IrScalar::Bool(false))),
            }),
            deferrable: Some(true),
            initially_deferred: None,
        },
    }
}

#[test]
fn postgres_renders_exclusion_constraint() {
    let mut live = BTreeSet::new();
    live.insert("bookings".to_string());
    let migrations = lower(
        vec![Op::AddConstraint {
            table: "bookings".into(),
            constraint: exclusion_constraint(),
            schema: None,
            existence_guard: None,
        }],
        SqlDialect::Postgres,
        &live,
    );
    assert_eq!(
        migrations[0].up,
        r#"ALTER TABLE "app"."bookings" ADD CONSTRAINT "bookings_no_overlap" EXCLUDE USING gist ("room" WITH =, "during" WITH &&) WHERE (("cancelled" = FALSE)) DEFERRABLE"#
    );
    assert_eq!(
        migrations[0].down.as_deref(),
        Some(r#"ALTER TABLE "app"."bookings" DROP CONSTRAINT "bookings_no_overlap""#)
    );
}

#[test]
fn postgres_parenthesizes_expression_exclusion_targets_only() {
    let mut live = BTreeSet::new();
    live.insert("bookings".to_string());
    let migrations = lower(
        vec![Op::AddConstraint {
            table: "bookings".into(),
            constraint: IrConstraint {
                name: Some("bookings_room_lower_excl".into()),
                kind: IrConstraintKind::Exclusion {
                    using_method: ExclusionMethod::Gist,
                    elements: vec![
                        ExclusionElement {
                            target: ColumnOrExpr::Column { name: "room".into() },
                            operator: ExclusionOperator::Equal,
                        },
                        ExclusionElement {
                            target: ColumnOrExpr::Expr {
                                expr: Expr::FnCall {
                                    r#fn: ScalarFn::Lower,
                                    args: vec![Expr::col("room")],
                                },
                            },
                            operator: ExclusionOperator::Equal,
                        },
                    ],
                    where_predicate: None,
                    deferrable: None,
                    initially_deferred: None,
                },
            },
            schema: None,
            existence_guard: None,
        }],
        SqlDialect::Postgres,
        &live,
    );

    assert_eq!(
        migrations[0].up,
        r#"ALTER TABLE "app"."bookings" ADD CONSTRAINT "bookings_room_lower_excl" EXCLUDE USING gist ("room" WITH =, (lower("room")) WITH =)"#
    );
    pg_query::parse(&migrations[0].up).expect("rendered exclusion expression target parses");
}

#[test]
fn sqlite_and_mysql_fail_closed_on_exclusion_constraints() {
    for dialect in [SqlDialect::Sqlite, SqlDialect::Mysql] {
        let err = IrAuthor::new(SCHEMA, OWNER, dialect)
            .lower(
                &ir(vec![Op::AddConstraint {
                    table: "bookings".into(),
                    constraint: exclusion_constraint(),
                    schema: None,
                    existence_guard: None,
                }]),
                &LiveSchema::from(&BTreeSet::from(["bookings".to_string()])),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            IrLowerError::ExclusionConstraintUnsupported {
                kind: "exclusionConstraint",
                dialect: d
            } if d == dialect
        ));
    }
}

#[test]
fn exclusion_operator_is_closed_at_deserialize() {
    let err = serde_json::from_value::<IrConstraintKind>(json!({
        "kind": "exclusion",
        "usingMethod": "gist",
        "elements": [
            {
                "target": { "kind": "column", "name": "room" },
                "operator": "~~"
            }
        ]
    }))
    .unwrap_err();
    assert!(
        err.to_string().contains("unknown variant"),
        "unexpected serde error: {err}"
    );
}
