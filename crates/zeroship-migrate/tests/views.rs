use std::collections::BTreeMap;

use zeroship_migrate::model::expr::{BinaryOp, Expr, UnaryOp};
use zeroship_migrate::guard::GuardConfig;
use zeroship_migrate::model::ir::{
    IrFlagsOverride, MigrationIr, Op, OrderDir, OrderItem, SelectAst, SelectItem, TableRef,
    ViewQuery, CURRENT_IR_VERSION,
};
use zeroship_migrate::render::lower::{IrAuthor, IrGuardedLowerError, IrLowerError, LiveSchema};
use zeroship_migrate::model::validate::{
    validate_ir_scoped, Dialect, UnsupportedKind, CODE_UNSUPPORTED, CODE_VENDOR_OP_DENIED,
};
use zeroship_migrate::{fold_ops, SchemaScope, SchemaSnapshot, ViewSnapshot};
use zeroship_schema::query::SqlDialect;

const SCHEMA: &str = "app";

fn ir(op: Op) -> MigrationIr {
    MigrationIr {
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
            select: active_users_select(),
        },
        replace,
        materialized,
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
    let author = IrAuthor::new(SCHEMA, "app_a", dialect).with_schema_scope(
        SchemaScope::Allowlist(vec![SCHEMA.to_string()]),
    );
    let migrations = author
        .lower(&ir(op), &LiveSchema::default())
        .map_err(Box::new)?;
    Ok(migrations[0].up.clone())
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
    let pg = lower_up(SqlDialect::Postgres, create_structured_view(Some(true), None)).unwrap();
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
    let pg = lower_up(SqlDialect::Postgres, create_structured_view(None, Some(true))).unwrap();
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
    assert!(lower_up(SqlDialect::Postgres, create_structured_view(Some(true), None)).is_ok());
    assert!(lower_up(SqlDialect::Postgres, create_structured_view(None, Some(true))).is_ok());
}

#[test]
fn plain_structured_view_is_confined_core_but_raw_view_is_capability_gated() {
    let structured = ir(create_structured_view(None, None));
    let confined = SchemaScope::Single(SCHEMA.to_string());
    validate_ir_scoped(&structured, Dialect::Postgres, &[], Some(&confined)).unwrap();

    let guard_cfg = GuardConfig::confined(SCHEMA);
    IrAuthor::new(SCHEMA, "app_a", SqlDialect::Postgres)
        .lower_guarded(&structured, &guard_cfg, &LiveSchema::default())
        .expect("plain structured view is core under confined lower_guarded");

    let raw = ir(raw_view("SELECT id FROM app.users", None));
    let err = validate_ir_scoped(&raw, Dialect::Postgres, &[], Some(&confined)).unwrap_err();
    assert_eq!(err.code, CODE_VENDOR_OP_DENIED);

    let err = IrAuthor::new(SCHEMA, "app_a", SqlDialect::Postgres)
        .lower_guarded(&raw, &guard_cfg, &LiveSchema::default())
        .unwrap_err();
    assert!(matches!(
        err,
        IrGuardedLowerError::Lower(IrLowerError::VendorCapabilityDenied {
            capability: zeroship_migrate::model::capability::VendorCapability::RawViewBody,
            ..
        })
    ));

    let operator = SchemaScope::Allowlist(vec![SCHEMA.to_string()]);
    validate_ir_scoped(&raw, Dialect::Postgres, &[], Some(&operator)).unwrap();
    IrAuthor::new(SCHEMA, "app_a", SqlDialect::Postgres)
        .with_schema_scope(operator)
        .lower(&raw, &LiveSchema::default())
        .expect("operator-capable lower admits raw view body");
}

#[test]
fn raw_view_body_must_be_single_top_level_select_even_with_capability() {
    let operator = SchemaScope::Allowlist(vec![SCHEMA.to_string()]);
    for sql in ["DROP TABLE x", "SELECT 1; DROP TABLE x"] {
        let err = validate_ir_scoped(&ir(raw_view(sql, None)), Dialect::Postgres, &[], Some(&operator))
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
        query: ViewQuery::Structured { select },
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
    let folded = fold_ops(std::slice::from_ref(&create), SqlDialect::Postgres, SCHEMA).unwrap();
    let mut expected_views = BTreeMap::new();
    expected_views.insert("active_users".to_string(), ViewSnapshot {
        materialized: false,
        columns: None,
        definition: None,
        comment: None,
    });
    assert_eq!(
        folded,
        SchemaSnapshot {
            tables: BTreeMap::new(),
            views: expected_views,
            ..Default::default()
        }
    );

    let drop = Op::DropView {
        name: "active_users".to_string(),
        schema: None,
        existence_guard: Some(zeroship_migrate::model::ir::ExistenceGuard::IfExists),
        materialized: None,
    };
    let folded = fold_ops(&[create, drop], SqlDialect::Postgres, SCHEMA).unwrap();
    assert!(folded.views.is_empty());
}
