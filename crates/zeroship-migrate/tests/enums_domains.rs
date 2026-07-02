use zeroship_migrate::{
    BinaryOp, ColType, Expr, IrColumn, IrDefault, IrFlagsOverride, IrScalar, IrAuthor,
    IrLowerError, LiveSchema, MigrationIr, Op, CURRENT_IR_VERSION,
};
use zeroship_migrate::{GuardDir, GuardProbe};
use zeroship_migrate::model::ir::ExistenceGuard;
use zeroship_schema::query::SqlDialect;

const SCHEMA: &str = "app";
const OWNER: &str = "app_a";

fn col(name: &str, ty: ColType) -> IrColumn {
    IrColumn {
        name: name.to_string(),
        ty,
        nullable: Some(true),
        default: None,
        unique: None,
        id_prefix: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
    }
}

fn ir(ops: Vec<Op>) -> MigrationIr {
    MigrationIr {
        ir_version: CURRENT_IR_VERSION,
        name: "enums_domains".to_string(),
        owner_app: OWNER.to_string(),
        ops,
        flags: IrFlagsOverride::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    }
}

fn create_table(name: &str, columns: Vec<IrColumn>) -> Op {
    Op::CreateTable {
        name: name.to_string(),
        columns,
        constraints: Vec::new(),
        indexes: Vec::new(),
        runtime_options: None,
            schema: None,
        existence_guard: None,
    }
}

fn domain_check() -> Expr {
    Expr::BinOp {
        op: BinaryOp::Ge,
        lhs: Box::new(Expr::col("VALUE")),
        rhs: Box::new(Expr::lit(IrScalar::Int(1))),
    }
}

fn create_enum() -> Op {
    Op::CreateEnum {
        name: "plan_tier".to_string(),
        schema: None,
        values: vec!["free".to_string(), "pro".to_string()],
    }
}

fn create_domain() -> Op {
    Op::CreateDomain {
        name: "billing_period".to_string(),
        schema: None,
        as_type: ColType::Int,
        check: Some(domain_check()),
        default: Some(IrDefault::Literal {
            value: IrScalar::Int(1),
        }),
        not_null: Some(true),
    }
}

fn lower_all(dialect: SqlDialect, ops: Vec<Op>) -> Vec<String> {
    let author = IrAuthor::new(SCHEMA, OWNER, dialect);
    author
        .lower(&ir(ops), &LiveSchema::default())
        .unwrap()
        .into_iter()
        .map(|m| m.up)
        .collect()
}

fn lower_create_table(dialect: SqlDialect, ops: Vec<Op>) -> String {
    lower_all(dialect, ops)
        .into_iter()
        .find(|sql| sql.starts_with("CREATE TABLE"))
        .expect("missing create table")
}

#[test]
fn pg_enum_and_domain_render_standalone_types_and_column_refs() {
    let sql = lower_all(
        SqlDialect::Postgres,
        vec![
            create_enum(),
            create_domain(),
            create_table(
                "subscriptions",
                vec![
                    col("tier", ColType::Enum {
                        name: "plan_tier".to_string(),
                    }),
                    col("period", ColType::Domain {
                        name: "billing_period".to_string(),
                    }),
                ],
            ),
        ],
    );

    assert_eq!(
        sql[0],
        r#"CREATE TYPE "app"."plan_tier" AS ENUM ('free', 'pro')"#
    );
    assert_eq!(
        sql[1],
        r#"CREATE DOMAIN "app"."billing_period" AS integer DEFAULT 1 NOT NULL CHECK ((VALUE >= 1))"#
    );
    assert!(
        sql[2].contains(r#""tier" "app"."plan_tier""#),
        "enum column must render as a Postgres type reference: {}",
        sql[2]
    );
    assert!(
        sql[2].contains(r#""period" "app"."billing_period""#),
        "domain column must render as a Postgres type reference: {}",
        sql[2]
    );
}

#[test]
fn sqlite_enum_and_domain_inline_at_column_use_site() {
    let sql = lower_create_table(
        SqlDialect::Sqlite,
        vec![
            create_enum(),
            create_domain(),
            create_table(
                "subscriptions",
                vec![
                    col("tier", ColType::Enum {
                        name: "plan_tier".to_string(),
                    }),
                    col("period", ColType::Domain {
                        name: "billing_period".to_string(),
                    }),
                ],
            ),
        ],
    );

    assert!(
        sql.contains(r#""tier" TEXT CHECK ("tier" IN ('free', 'pro'))"#),
        "SQLite enum must inline TEXT + CHECK at the column: {sql}"
    );
    assert!(
        sql.contains(r#""period" INTEGER NOT NULL DEFAULT 1 CHECK (("period" >= 1))"#),
        "SQLite domain must inline base type + default/not-null/check: {sql}"
    );
}

#[test]
fn mysql_enum_and_domain_inline_at_column_use_site() {
    let sql = lower_create_table(
        SqlDialect::Mysql,
        vec![
            create_enum(),
            create_domain(),
            create_table(
                "subscriptions",
                vec![
                    col("tier", ColType::Enum {
                        name: "plan_tier".to_string(),
                    }),
                    col("period", ColType::Domain {
                        name: "billing_period".to_string(),
                    }),
                ],
            ),
        ],
    );

    assert!(
        sql.contains("`tier` ENUM('free', 'pro')"),
        "MySQL enum must inline native ENUM at the column: {sql}"
    );
    assert!(
        sql.contains("`period` INT NOT NULL DEFAULT 1 CHECK ((`period` >= 1))"),
        "MySQL domain must inline base type + default/not-null/check: {sql}"
    );
}

#[test]
fn mysql_named_type_reference_outside_inline_create_add_fails_closed() {
    let author = IrAuthor::new(SCHEMA, OWNER, SqlDialect::Mysql);
    let err = author
        .lower(
            &ir(vec![
                create_enum(),
                Op::SetColumnType {
                    table: "subscriptions".to_string(),
                    column: "tier".to_string(),
                    to_type: ColType::Enum {
                        name: "plan_tier".to_string(),
                    },
                    using: None,
                    schema: None,
                    existence_guard: None,
                },
            ]),
            &LiveSchema::default(),
        )
        .unwrap_err();

    assert!(matches!(
        err,
        IrLowerError::NamedTypeUnsupported {
            kind: "enum",
            reason: "unreachable use-site",
            ..
        }
    ));
}

#[test]
fn pg_guarded_type_drops_stamp_named_type_probes() {
    let author = IrAuthor::new(SCHEMA, OWNER, SqlDialect::Postgres);
    let migrations = author
        .lower(
            &ir(vec![
                Op::DropEnum {
                    name: "plan_tier".to_string(),
                    schema: None,
                    existence_guard: Some(ExistenceGuard::IfExists),
                },
                Op::DropDomain {
                    name: "billing_period".to_string(),
                    schema: None,
                    existence_guard: Some(ExistenceGuard::IfExists),
                },
            ]),
            &LiveSchema::default(),
        )
        .unwrap();

    assert_eq!(migrations[0].up, r#"DROP TYPE "app"."plan_tier""#);
    assert_eq!(migrations[1].up, r#"DROP DOMAIN "app"."billing_period""#);
    assert!(matches!(
        migrations[0].existence_guard.as_ref(),
        Some(GuardProbe::NamedType { name, kind, direction, .. })
            if name == "plan_tier" && kind == "enum" && *direction == GuardDir::IfExists
    ));
    assert!(matches!(
        migrations[1].existence_guard.as_ref(),
        Some(GuardProbe::NamedType { name, kind, direction, .. })
            if name == "billing_period" && kind == "domain" && *direction == GuardDir::IfExists
    ));
}
