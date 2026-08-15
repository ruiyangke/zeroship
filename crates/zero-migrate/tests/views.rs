mod support;

use std::collections::BTreeMap;

use zero_migrate::guard::GuardConfig;
use zero_migrate::model::expr::{AggFunc, BinaryOp, Expr, UnaryOp};
use zero_migrate::model::ir::{
    IrFlagsOverride, IrScalar, MigrationIr, Op, OrderDir, OrderItem, SafeU64, SelectAst,
    SelectItem, TableRef, ViewQuery, CURRENT_IR_VERSION,
};
use zero_migrate::model::validate::{
    validate_ir_scoped, Dialect, UnsupportedKind, CODE_AGGREGATE_IN_SCALAR_CONTEXT,
    CODE_UNSUPPORTED, CODE_VENDOR_OP_DENIED,
};
use zero_migrate::render::lower::{IrAuthor, IrGuardedLowerError, IrLowerError, LiveSchema};
use zero_migrate::schema::query::SqlDialect;
use zero_migrate::{fold_ops, SchemaScope, SchemaSnapshot, ViewSnapshot};

const SCHEMA: &str = "app";

fn ir(op: Op) -> MigrationIr {
    MigrationIr {
        inverse_ops: None,
        irreversible: None,
        ir_version: CURRENT_IR_VERSION,
        name: "views".to_string(),
        owner_app: "app_a".to_string(),
        ops: vec![op],
        flags: IrFlagsOverride::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    }
}

fn active_users_select() -> SelectAst {
    SelectAst {
        from: TableRef {
            name: "users".to_string(),
            schema: None,
            alias: None,
        },
        projection: vec![
            SelectItem::ColRef {
                table: None,
                name: "id".to_string(),
                alias: None,
            },
            SelectItem::ColRef {
                table: None,
                name: "email".to_string(),
                alias: None,
            },
        ],
        joins: Vec::new(),
        r#where: Some(Expr::UnaryOp {
            op: UnaryOp::IsNull,
            operand: Box::new(Expr::col("deleted_at")),
        }),
        group_by: Vec::new(),
        having: None,
        order_by: None,
        limit: None,
    }
}

fn create_structured_view(replace: Option<bool>, materialized: Option<bool>) -> Op {
    Op::CreateView {
        name: "active_users".to_string(),
        schema: None,
        columns: None,
        query: ViewQuery::Structured {
            select: Box::new(active_users_select()),
        },
        replace,
        materialized,
    }
}

const fn count_star() -> Expr {
    Expr::Agg {
        func: AggFunc::Count,
        arg: None,
        delimiter: None,
        distinct: false,
    }
}

fn count(expr: Expr) -> Expr {
    Expr::Agg {
        func: AggFunc::Count,
        arg: Some(Box::new(expr)),
        delimiter: None,
        distinct: false,
    }
}

fn sum(expr: Expr) -> Expr {
    Expr::Agg {
        func: AggFunc::Sum,
        arg: Some(Box::new(expr)),
        delimiter: None,
        distinct: false,
    }
}

fn string_agg(expr: Expr, delimiter: Expr) -> Expr {
    Expr::Agg {
        func: AggFunc::StringAgg,
        arg: Some(Box::new(expr)),
        delimiter: Some(Box::new(delimiter)),
        distinct: false,
    }
}

fn array_agg(expr: Expr) -> Expr {
    Expr::Agg {
        func: AggFunc::ArrayAgg,
        arg: Some(Box::new(expr)),
        delimiter: None,
        distinct: false,
    }
}

fn bool_and(expr: Expr) -> Expr {
    Expr::Agg {
        func: AggFunc::BoolAnd,
        arg: Some(Box::new(expr)),
        delimiter: None,
        distinct: false,
    }
}

fn grouped_order_totals_view() -> Op {
    Op::CreateView {
        name: "order_totals".to_string(),
        schema: None,
        columns: None,
        query: ViewQuery::Structured {
            select: Box::new(SelectAst {
                from: TableRef {
                    name: "orders".to_string(),
                    schema: None,
                    alias: None,
                },
                projection: vec![
                    SelectItem::ColRef {
                        table: None,
                        name: "customer_id".to_string(),
                        alias: None,
                    },
                    SelectItem::Expr {
                        expr: count_star(),
                        alias: Some("n".to_string()),
                    },
                    SelectItem::Expr {
                        expr: sum(Expr::col("amount")),
                        alias: Some("revenue".to_string()),
                    },
                ],
                joins: Vec::new(),
                r#where: Some(Expr::BinOp {
                    op: BinaryOp::Eq,
                    lhs: Box::new(Expr::col("status")),
                    rhs: Box::new(Expr::lit(IrScalar::Str("paid".to_string()))),
                }),
                group_by: vec![Expr::col("customer_id")],
                having: Some(Expr::BinOp {
                    op: BinaryOp::Gt,
                    lhs: Box::new(count(Expr::col("id"))),
                    rhs: Box::new(Expr::lit(IrScalar::Int(5))),
                }),
                order_by: Some(vec![OrderItem::ColRef {
                    table: None,
                    name: "customer_id".to_string(),
                    dir: Some(OrderDir::Asc),
                }]),
                limit: Some(SafeU64::new(10).unwrap()),
            }),
        },
        replace: None,
        materialized: None,
    }
}

fn pg_first_aggregate_rollup_view() -> Op {
    Op::CreateView {
        name: "order_rollups".to_string(),
        schema: None,
        columns: None,
        query: ViewQuery::Structured {
            select: Box::new(SelectAst {
                from: TableRef {
                    name: "orders".to_string(),
                    schema: None,
                    alias: None,
                },
                projection: vec![
                    SelectItem::ColRef {
                        table: None,
                        name: "customer_id".to_string(),
                        alias: None,
                    },
                    SelectItem::Expr {
                        expr: string_agg(
                            Expr::col("item_name"),
                            Expr::lit(IrScalar::Str(", ".to_string())),
                        ),
                        alias: Some("item_names".to_string()),
                    },
                    SelectItem::Expr {
                        expr: array_agg(Expr::col("id")),
                        alias: Some("order_ids".to_string()),
                    },
                    SelectItem::Expr {
                        expr: bool_and(Expr::col("fulfilled")),
                        alias: Some("all_fulfilled".to_string()),
                    },
                ],
                joins: Vec::new(),
                r#where: None,
                group_by: vec![Expr::col("customer_id")],
                having: Some(Expr::BinOp {
                    op: BinaryOp::Gt,
                    lhs: Box::new(count(Expr::col("id"))),
                    rhs: Box::new(Expr::lit(IrScalar::Int(1))),
                }),
                order_by: None,
                limit: None,
            }),
        },
        replace: None,
        materialized: None,
    }
}

fn raw_view(sql: &str, materialized: Option<bool>) -> Op {
    Op::CreateView {
        name: "raw_active_users".to_string(),
        schema: None,
        columns: None,
        query: ViewQuery::Raw {
            sql: sql.to_string(),
        },
        replace: None,
        materialized,
    }
}

fn lower_up(dialect: SqlDialect, op: Op) -> Result<String, Box<IrLowerError>> {
    // The operator charter grants the vendor capabilities a materialized view / raw
    // view body needs; the widened scope stays because the view body's table
    // references are confined against it.
    let author = IrAuthor::new(SCHEMA, "app_a", dialect, &support::operator_charter("app"))
        .with_schema_scope(SchemaScope::Allowlist(vec![SCHEMA.to_string()]));
    let migrations = author
        .lower(&ir(op), &LiveSchema::default())
        .map_err(Box::new)?;
    Ok(migrations[0].up.clone())
}

#[test]
fn structured_select_supports_group_by_and_having_on_all_dialects() {
    let pg = lower_up(SqlDialect::Postgres, grouped_order_totals_view()).unwrap();
    assert_eq!(
        pg,
        "CREATE VIEW \"app\".\"order_totals\" AS SELECT \"customer_id\", count(*) AS \"n\", sum(\"amount\") AS \"revenue\" FROM \"app\".\"orders\" WHERE (\"status\" = 'paid') GROUP BY \"customer_id\" HAVING (count(\"id\") > 5) ORDER BY \"customer_id\" ASC LIMIT 10"
    );

    let sqlite = lower_up(SqlDialect::Sqlite, grouped_order_totals_view()).unwrap();
    assert_eq!(
        sqlite,
        "CREATE VIEW \"order_totals\" AS SELECT \"customer_id\", count(*) AS \"n\", sum(\"amount\") AS \"revenue\" FROM \"orders\" WHERE (\"status\" = 'paid') GROUP BY \"customer_id\" HAVING (count(\"id\") > 5) ORDER BY \"customer_id\" ASC LIMIT 10"
    );

    let mysql = lower_up(SqlDialect::Mysql, grouped_order_totals_view()).unwrap();
    assert_eq!(
        mysql,
        "CREATE VIEW `app`.`order_totals` AS SELECT `customer_id`, count(*) AS `n`, sum(`amount`) AS `revenue` FROM `app`.`orders` WHERE (`status` = _utf8mb4 X'70616964') GROUP BY `customer_id` HAVING (count(`id`) > 5) ORDER BY `customer_id` ASC LIMIT 10"
    );
}

#[test]
fn structured_select_allows_aggregates_in_projection_and_having() {
    let trusted = SchemaScope::Unconfined;
    validate_ir_scoped(
        &ir(grouped_order_totals_view()),
        Dialect::Postgres,
        &[],
        Some(&trusted),
    )
    .expect("projection and HAVING are grouped SELECT contexts and allow aggregates");
}

#[test]
fn pg_first_aggregate_view_renders_on_postgres_and_refuses_off_pg() {
    let pg = lower_up(SqlDialect::Postgres, pg_first_aggregate_rollup_view()).unwrap();
    assert_eq!(
        pg,
        "CREATE VIEW \"app\".\"order_rollups\" AS SELECT \"customer_id\", string_agg(\"item_name\", ', ') AS \"item_names\", array_agg(\"id\") AS \"order_ids\", bool_and(\"fulfilled\") AS \"all_fulfilled\" FROM \"app\".\"orders\" GROUP BY \"customer_id\" HAVING (count(\"id\") > 1)"
    );

    let trusted = SchemaScope::Unconfined;
    for dialect in [Dialect::Sqlite, Dialect::Mysql] {
        let err = validate_ir_scoped(
            &ir(pg_first_aggregate_rollup_view()),
            dialect,
            &[],
            Some(&trusted),
        )
        .unwrap_err();
        assert_eq!(
            err.code,
            zero_migrate::model::validate::CODE_DIALECT_UNSUPPORTED
        );
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
    }
}

#[test]
fn structured_select_rejects_aggregate_group_by_item() {
    let mut op = grouped_order_totals_view();
    let Op::CreateView {
        query: ViewQuery::Structured { select },
        ..
    } = &mut op
    else {
        unreachable!("grouped_order_totals_view returns a structured createView");
    };
    select.group_by = vec![count(Expr::col("id"))];

    let trusted = SchemaScope::Unconfined;
    let err = validate_ir_scoped(&ir(op), Dialect::Postgres, &[], Some(&trusted)).unwrap_err();
    assert_eq!(err.code, CODE_AGGREGATE_IN_SCALAR_CONTEXT);
    assert!(
        err.reason.contains("GROUP BY") && err.reason.contains("count"),
        "reason should name GROUP BY aggregate misuse: {err:?}"
    );
}

#[test]
fn pg_structured_view_renders_exact_select_where() {
    let up = lower_up(SqlDialect::Postgres, create_structured_view(None, None)).unwrap();
    assert_eq!(
        up,
        "CREATE VIEW \"app\".\"active_users\" AS SELECT \"id\", \"email\" FROM \"app\".\"users\" WHERE (\"deleted_at\" IS NULL)"
    );
}

#[test]
fn replace_renders_or_replace_on_pg_and_drop_create_on_sqlite() {
    let pg = lower_up(
        SqlDialect::Postgres,
        create_structured_view(Some(true), None),
    )
    .unwrap();
    assert_eq!(
        pg,
        "CREATE OR REPLACE VIEW \"app\".\"active_users\" AS SELECT \"id\", \"email\" FROM \"app\".\"users\" WHERE (\"deleted_at\" IS NULL)"
    );

    let sqlite = lower_up(SqlDialect::Sqlite, create_structured_view(Some(true), None)).unwrap();
    assert_eq!(
        sqlite,
        "DROP VIEW IF EXISTS \"active_users\";\nCREATE VIEW \"active_users\" AS SELECT \"id\", \"email\" FROM \"users\" WHERE (\"deleted_at\" IS NULL)"
    );
}

#[test]
fn sqlite_structured_view_renders_dialect_quoted_select() {
    let up = lower_up(SqlDialect::Sqlite, create_structured_view(None, None)).unwrap();
    assert_eq!(
        up,
        "CREATE VIEW \"active_users\" AS SELECT \"id\", \"email\" FROM \"users\" WHERE (\"deleted_at\" IS NULL)"
    );
}

#[test]
fn materialized_view_renders_on_pg_and_is_unsupported_on_sqlite() {
    let pg = lower_up(
        SqlDialect::Postgres,
        create_structured_view(None, Some(true)),
    )
    .unwrap();
    assert_eq!(
        pg,
        "CREATE MATERIALIZED VIEW \"app\".\"active_users\" AS SELECT \"id\", \"email\" FROM \"app\".\"users\" WHERE (\"deleted_at\" IS NULL)"
    );

    let trusted = SchemaScope::Unconfined;
    let err = validate_ir_scoped(
        &ir(create_structured_view(None, Some(true))),
        Dialect::Sqlite,
        &[],
        Some(&trusted),
    )
    .unwrap_err();
    assert_eq!(err.code, CODE_UNSUPPORTED);
    assert_eq!(err.kind, Some(UnsupportedKind::Op));
    assert!(err.reason.contains("materialized"));
}

#[test]
fn replace_plus_materialized_is_rejected_on_pg_not_silently_dropped() {
    // SA-13: Postgres has no CREATE OR REPLACE MATERIALIZED VIEW. The renderer
    // must fail closed rather than silently emit a plain CREATE MATERIALIZED VIEW
    // (which would drop the `replace` request) or destructively DROP+CREATE.
    let trusted = SchemaScope::Unconfined;
    let err = validate_ir_scoped(
        &ir(create_structured_view(Some(true), Some(true))),
        Dialect::Postgres,
        &[],
        Some(&trusted),
    )
    .unwrap_err();
    assert_eq!(err.code, CODE_UNSUPPORTED);
    assert_eq!(err.kind, Some(UnsupportedKind::Op));
    assert!(err.reason.contains("replace+materialized"));
    // The non-contradictory shapes still lower fine.
    assert!(lower_up(
        SqlDialect::Postgres,
        create_structured_view(Some(true), None)
    )
    .is_ok());
    assert!(lower_up(
        SqlDialect::Postgres,
        create_structured_view(None, Some(true))
    )
    .is_ok());
}

#[test]
fn plain_structured_view_is_confined_core_but_raw_view_is_capability_gated() {
    let structured = ir(create_structured_view(None, None));
    let confined = SchemaScope::Single(SCHEMA.to_string());
    validate_ir_scoped(&structured, Dialect::Postgres, &[], Some(&confined)).unwrap();

    let guard_cfg = GuardConfig::from_policy(support::no_inject(SCHEMA), SqlDialect::Postgres);
    IrAuthor::new(
        SCHEMA,
        "app_a",
        SqlDialect::Postgres,
        &support::no_inject("app"),
    )
    .lower_guarded(&structured, &guard_cfg, &LiveSchema::default())
    .expect("plain structured view is core under confined lower_guarded");

    let raw = ir(raw_view("SELECT id FROM app.users", None));
    let err = validate_ir_scoped(&raw, Dialect::Postgres, &[], Some(&confined)).unwrap_err();
    assert_eq!(err.code, CODE_VENDOR_OP_DENIED);

    let err = IrAuthor::new(
        SCHEMA,
        "app_a",
        SqlDialect::Postgres,
        &support::no_inject("app"),
    )
    .lower_guarded(&raw, &guard_cfg, &LiveSchema::default())
    .unwrap_err();
    assert!(matches!(
        err,
        IrGuardedLowerError::Lower(IrLowerError::VendorCapabilityDenied {
            capability: zero_migrate::model::capability::VendorCapability::RawViewBody,
            ..
        })
    ));

    let operator = SchemaScope::Allowlist(vec![SCHEMA.to_string()]);
    validate_ir_scoped(&raw, Dialect::Postgres, &[], Some(&operator)).unwrap();
    // The `sql.raw_view_body` GRANT is what admits this at lower - the same charter
    // the guard above would compose, not the widened scope beside it.
    IrAuthor::new(
        SCHEMA,
        "app_a",
        SqlDialect::Postgres,
        &support::operator_charter("app"),
    )
    .with_schema_scope(operator)
    .lower(&raw, &LiveSchema::default())
    .expect("operator-capable lower admits raw view body");
}

#[test]
fn raw_view_body_must_be_single_top_level_select_even_with_capability() {
    let operator = SchemaScope::Allowlist(vec![SCHEMA.to_string()]);
    for sql in ["DROP TABLE x", "SELECT 1; DROP TABLE x"] {
        let err = validate_ir_scoped(
            &ir(raw_view(sql, None)),
            Dialect::Postgres,
            &[],
            Some(&operator),
        )
        .unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert!(
            err.reason.contains("single top-level SELECT")
                || err.reason.contains("exactly one top-level SELECT"),
            "{err:?}"
        );
    }
}

#[test]
fn raw_view_body_runs_function_body_deny_list_scan() {
    let operator = SchemaScope::Allowlist(vec![SCHEMA.to_string()]);
    let err = validate_ir_scoped(
        &ir(raw_view("SELECT pg_read_file('/etc/passwd')", None)),
        Dialect::Postgres,
        &[],
        Some(&operator),
    )
    .unwrap_err();
    assert_eq!(err.code, CODE_UNSUPPORTED);
    assert!(err.reason.contains("body scanner"), "{err:?}");
}

#[test]
fn structured_select_supports_order_limit_and_closed_expr_projection() {
    let select = SelectAst {
        from: TableRef {
            name: "users".to_string(),
            schema: None,
            alias: Some("u".to_string()),
        },
        projection: vec![SelectItem::Expr {
            expr: Expr::BinOp {
                op: BinaryOp::Concat,
                lhs: Box::new(Expr::col("first_name")),
                rhs: Box::new(Expr::col("last_name")),
            },
            alias: Some("display_name".to_string()),
        }],
        joins: Vec::new(),
        r#where: None,
        group_by: Vec::new(),
        having: None,
        order_by: Some(vec![OrderItem::ColRef {
            table: Some("u".to_string()),
            name: "created_at".to_string(),
            dir: Some(OrderDir::Desc),
        }]),
        limit: None,
    };
    let op = Op::CreateView {
        name: "user_names".to_string(),
        schema: None,
        columns: Some(vec!["display_name".to_string()]),
        query: ViewQuery::Structured {
            select: Box::new(select),
        },
        replace: None,
        materialized: None,
    };
    let up = lower_up(SqlDialect::Postgres, op).unwrap();
    assert_eq!(
        up,
        "CREATE VIEW \"app\".\"user_names\" (\"display_name\") AS SELECT (\"first_name\" || \"last_name\") AS \"display_name\" FROM \"app\".\"users\" AS \"u\" ORDER BY \"u\".\"created_at\" DESC"
    );
}

#[test]
fn fold_records_views_and_drop_removes_them() {
    let create = create_structured_view(None, None);
    let folded = fold_ops(
        std::slice::from_ref(&create),
        SqlDialect::Postgres,
        SCHEMA,
        &support::no_inject("app"),
    )
    .unwrap();
    let mut expected_views = BTreeMap::new();
    expected_views.insert(
        "active_users".to_string(),
        ViewSnapshot {
            materialized: false,
            columns: None,
            definition: None,
            authored_query: Some(ViewQuery::Structured {
                select: Box::new(active_users_select()),
            }),
            authored_schema: None,
            comment: None,
        },
    );
    assert_eq!(
        folded,
        SchemaSnapshot {
            tables: BTreeMap::new(),
            views: expected_views,
            ..Default::default()
        }
    );

    // `ViewSnapshot` equality compares only `materialized` and `comment`, so the
    // assertion above cannot see the retained body at all. Read it directly: a
    // later `dropView` renders its inverse from this, and nothing else proves the
    // fold kept it.
    assert_eq!(
        folded.views["active_users"].authored_query,
        Some(ViewQuery::Structured {
            select: Box::new(active_users_select()),
        }),
        "the fold must retain the authored view body for the drop to invert"
    );

    let drop = Op::DropView {
        name: "active_users".to_string(),
        schema: None,
        existence_guard: Some(zero_migrate::model::ir::ExistenceGuard::IfExists),
        materialized: None,
    };
    let folded = fold_ops(
        &[create, drop],
        SqlDialect::Postgres,
        SCHEMA,
        &support::no_inject("app"),
    )
    .unwrap();
    assert!(folded.views.is_empty());
}
