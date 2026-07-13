use std::collections::BTreeSet;

use serde_json::json;
use zero_migrate::model::ir::ExistenceGuard;
use zero_migrate::model::profile::PolicyProfile;
use zero_migrate::model::validate::{
    validate_ir, validate_ir_scoped, Dialect, UnsupportedKind, CODE_UNSUPPORTED,
};
use zero_migrate::render::lower::IrAuthor;
use zero_migrate::{
    fold_ops, BinaryOp, ColType, ColumnOrExpr, CommentTarget, ExclusionElement,
    ExclusionMethod, ExclusionOperator, Expr, IndexElement, IrConstraint, IrConstraintKind,
    IrColumn, IrDefault, IrScalar, LiveSchema, MigrationIr, Op, ScalarFn, SequenceOwnedBy,
    SequenceRef, UnaryOp,
    SafeI64, SafeU64,
};
use zero_migrate::schema::query::SqlDialect;

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

fn lower(ops: Vec<Op>, dialect: SqlDialect, live: &BTreeSet<String>) -> Vec<zero_migrate::Migration> {
    let author = IrAuthor::new(SCHEMA, OWNER, dialect);
    author
        .lower(&ir(ops), &LiveSchema::from(live))
        .expect("lower")
}

fn si(n: i64) -> SafeI64 {
    SafeI64::new(n).expect("test sequence value is JS-safe")
}

fn su(n: u64) -> SafeU64 {
    SafeU64::new(n).expect("test sequence cache is JS-safe")
}

fn create_sequence_op() -> Op {
    Op::CreateSequence {
        name: "invoice_seq".into(),
        schema: None,
        as_type: Some(ColType::BigInt),
        increment: Some(si(5)),
        start: Some(si(100)),
        min_value: None,
        max_value: None,
        cache: Some(su(10)),
        cycle: Some(true),
        owned_by: Some(Some(SequenceOwnedBy {
            table: "invoices".into(),
            column: "id".into(),
        })),
    }
}

fn nextval_col(name: &str, schema: Option<&str>) -> IrColumn {
    IrColumn {
        name: name.into(),
        ty: ColType::BigInt,
        nullable: Some(false),
        default: Some(IrDefault::Nextval {
            sequence: SequenceRef {
                name: "invoice_seq".into(),
                schema: schema.map(str::to_string),
            },
        }),
        unique: None,
        id_prefix: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
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
            increment: Some(si(7)),
            restart: Some(Some(si(200))),
            min_value: Some(Some(si(1))),
            max_value: Some(Some(si(999))),
            cache: Some(su(20)),
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
fn postgres_renders_nextval_default_with_and_without_schema() {
    let with_schema = lower(
        vec![Op::CreateTable {
            name: "invoices".into(),
            columns: vec![nextval_col("id", Some("app"))],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        }],
        SqlDialect::Postgres,
        &BTreeSet::new(),
    );
    assert!(
        with_schema[0].up.contains("DEFAULT nextval('app.invoice_seq'::regclass)"),
        "schema-qualified nextval default must render pg_dump-style: {}",
        with_schema[0].up
    );

    let without_schema = lower(
        vec![Op::CreateTable {
            name: "invoices".into(),
            columns: vec![nextval_col("id", None)],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        }],
        SqlDialect::Postgres,
        &BTreeSet::new(),
    );
    assert!(
        without_schema[0].up.contains("DEFAULT nextval('invoice_seq'::regclass)"),
        "unqualified nextval default must render pg_dump-style: {}",
        without_schema[0].up
    );
}

#[test]
fn postgres_renders_valid_descending_sequence() {
    let create = lower(
        vec![Op::CreateSequence {
            name: "descending_seq".into(),
            schema: None,
            as_type: Some(ColType::Int),
            increment: Some(si(-5)),
            start: Some(si(100)),
            min_value: Some(Some(si(-100))),
            max_value: Some(Some(si(100))),
            cache: Some(su(1)),
            cycle: Some(false),
            owned_by: None,
        }],
        SqlDialect::Postgres,
        &BTreeSet::new(),
    );
    assert_eq!(
        create[0].up,
        r#"CREATE SEQUENCE "app"."descending_seq" AS integer INCREMENT BY -5 START WITH 100 MINVALUE -100 MAXVALUE 100 CACHE 1 NO CYCLE"#
    );
}

#[test]
fn sqlite_and_mysql_fail_closed_on_sequences() {
    for dialect in [Dialect::Sqlite, Dialect::Mysql] {
        let err = validate_ir(&ir(vec![create_sequence_op()]), dialect, &[]).unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Op));
        assert!(err.reason.contains("sequence"));
    }
}

#[test]
fn nextval_default_rejects_non_integer_and_non_postgres() {
    let text_nextval = Op::CreateTable {
        name: "events".into(),
        columns: vec![IrColumn {
            ty: ColType::Text,
            ..nextval_col("counter", Some("app"))
        }],
        primary_key: None,
        constraints: vec![],
        indexes: vec![],
        partition_by: None,
        runtime_options: None,
        schema: None,
        existence_guard: None,
    };
    // Validate under the platform profile (author-PK allowed → the createTable
    // table-shape gate returns Ok), so validation reaches the column-default-type
    // check — the realistic profile, since nextval defaults are used on the platform.
    let err = validate_ir_scoped(
        &ir(vec![text_nextval.clone()]),
        Dialect::Postgres,
        &[],
        None,
        &PolicyProfile::platform(),
    )
    .unwrap_err();
    assert_eq!(err.code, zero_migrate::model::validate::CODE_COLUMN_DEFAULT_TYPE);
    assert!(err.reason.contains("nextval defaults require an integer column"));

    for dialect in [Dialect::Sqlite, Dialect::Mysql] {
        // Platform profile so the createTable table-shape gate does not pre-empt the
        // dialect-level unsupported check (nextval defaults are PostgreSQL-only).
        let err = validate_ir_scoped(
            &ir(vec![text_nextval.clone()]),
            dialect,
            &[],
            None,
            &PolicyProfile::platform(),
        )
        .unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Op));
        assert!(err.reason.contains("nextval"));
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

#[test]
fn postgres_renders_comment_on_all_structured_targets() {
    let migrations = lower(
        vec![
            Op::Comment {
                target: CommentTarget::Table {
                    schema: Some("app".into()),
                    name: "accounts".into(),
                },
                comment: Some("Accounts' table".into()),
            },
            Op::Comment {
                target: CommentTarget::Column {
                    schema: None,
                    table: "users".into(),
                    name: "email".into(),
                },
                comment: Some("Login email".into()),
            },
            Op::Comment {
                target: CommentTarget::Index { schema: None, name: "users_email_idx".into() },
                comment: Some("Email lookup".into()),
            },
            Op::Comment {
                target: CommentTarget::Constraint {
                    schema: None,
                    table: "users".into(),
                    name: "users_email_uq".into(),
                },
                comment: Some("Email uniqueness".into()),
            },
            Op::Comment {
                target: CommentTarget::View { schema: None, name: "active_users".into() },
                comment: Some("Active users".into()),
            },
            Op::Comment {
                target: CommentTarget::Type { schema: None, name: "user_status".into() },
                comment: Some("Allowed states".into()),
            },
            Op::Comment {
                target: CommentTarget::Sequence { schema: None, name: "invoice_seq".into() },
                comment: None,
            },
            Op::Comment {
                target: CommentTarget::Function {
                    schema: None,
                    name: "normalize_email".into(),
                },
                comment: Some("Normalize email".into()),
            },
        ],
        SqlDialect::Postgres,
        &BTreeSet::new(),
    );
    let up: Vec<&str> = migrations.iter().map(|m| m.up.as_str()).collect();
    assert_eq!(
        up,
        vec![
            r#"COMMENT ON TABLE "app"."accounts" IS 'Accounts'' table'"#,
            r#"COMMENT ON COLUMN "app"."users"."email" IS 'Login email'"#,
            r#"COMMENT ON INDEX "app"."users_email_idx" IS 'Email lookup'"#,
            r#"COMMENT ON CONSTRAINT "users_email_uq" ON "app"."users" IS 'Email uniqueness'"#,
            r#"COMMENT ON VIEW "app"."active_users" IS 'Active users'"#,
            r#"COMMENT ON TYPE "app"."user_status" IS 'Allowed states'"#,
            r#"COMMENT ON SEQUENCE "app"."invoice_seq" IS NULL"#,
            r#"COMMENT ON FUNCTION "app"."normalize_email" IS 'Normalize email'"#,
        ]
    );
    assert!(migrations.iter().all(|m| m.down.is_none()));
}

#[test]
fn sqlite_and_mysql_fail_closed_on_comment_on() {
    for dialect in [Dialect::Sqlite, Dialect::Mysql] {
        let err = validate_ir(
            &ir(vec![Op::Comment {
                target: CommentTarget::Table { schema: None, name: "users".into() },
                comment: Some("Users".into()),
            }]),
            dialect,
            &[],
        )
        .unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Op));
        assert!(err.reason.contains("COMMENT ON"));
    }
}

#[test]
fn fold_tracks_and_clears_table_and_column_comments() {
    let base: MigrationIr = serde_json::from_str(
        r#"{"ir_version":1,"name":"comments","ops":[
            {"op":"createTable","name":"users","columns":[
                {"name":"email","type":"text","nullable":false}
            ]}
        ]}"#,
    )
    .unwrap();

    let mut set_ops = base.ops;
    set_ops.push(Op::Comment {
        target: CommentTarget::Table { schema: None, name: "users".into() },
        comment: Some("User accounts".into()),
    });
    set_ops.push(Op::Comment {
        target: CommentTarget::Column {
            schema: None,
            table: "users".into(),
            name: "email".into(),
        },
        comment: Some("Login email".into()),
    });

    let folded = fold_ops(&set_ops, SqlDialect::Postgres, SCHEMA).expect("fold set comments");
    let users = folded.tables.get("users").expect("users table");
    assert_eq!(users.comment.as_deref(), Some("User accounts"));
    assert_eq!(
        users
            .columns
            .iter()
            .find(|c| c.name == "email")
            .and_then(|c| c.comment.as_deref()),
        Some("Login email")
    );

    let mut cleared_ops = set_ops;
    cleared_ops.push(Op::Comment {
        target: CommentTarget::Table { schema: None, name: "users".into() },
        comment: None,
    });
    cleared_ops.push(Op::Comment {
        target: CommentTarget::Column {
            schema: None,
            table: "users".into(),
            name: "email".into(),
        },
        comment: None,
    });
    let cleared = fold_ops(&cleared_ops, SqlDialect::Postgres, SCHEMA).expect("fold clear comments");
    let users = cleared.tables.get("users").expect("users table");
    assert_eq!(users.comment, None);
    assert_eq!(
        users
            .columns
            .iter()
            .find(|c| c.name == "email")
            .and_then(|c| c.comment.as_deref()),
        None
    );
}

fn idx_col(name: &str) -> IndexElement {
    IndexElement::Column {
        name: name.to_string(),
        order: None,
        opclass: None,
        collation: None,
    }
}

fn lower_email_expr() -> Expr {
    Expr::FnCall { r#fn: ScalarFn::Lower, args: vec![Expr::col("email")] }
}

fn active_true_expr() -> Expr {
    Expr::UnaryOp { op: UnaryOp::IsTrue, operand: Box::new(Expr::col("active")) }
}

#[test]
fn postgres_and_sqlite_render_partial_index_where() {
    let op = Op::CreateIndex {
        table: "users".into(),
        columns: vec![idx_col("active")],
        name: Some("users_active_idx".into()),
        unique: None,
        using: None,
        r#where: Some(active_true_expr()),

    include: Vec::new(),
    with: None,
    only: None,
    concurrently: None,
        schema: None,
        existence_guard: None,
        nulls_not_distinct: None,
    };
    let live = BTreeSet::from(["users".to_string()]);

    let pg = lower(vec![op.clone()], SqlDialect::Postgres, &live);
    assert_eq!(
        pg[0].up,
        r#"CREATE INDEX IF NOT EXISTS "users_active_idx" ON "app"."users" ("active") WHERE ("active" IS TRUE)"#
    );

    let sqlite = lower(vec![op], SqlDialect::Sqlite, &live);
    assert_eq!(
        sqlite[0].up,
        r#"CREATE INDEX IF NOT EXISTS "users_active_idx" ON "users" ("active") WHERE ("active" = 1)"#
    );
}

#[test]
fn postgres_and_sqlite_render_expression_index_elements() {
    let op = Op::CreateIndex {
        table: "users".into(),
        columns: vec![idx_col("email"), IndexElement::Expr { expr: lower_email_expr() }],
        name: Some("users_email_lower_idx".into()),
        unique: None,
        using: None,
        r#where: Some(active_true_expr()),

    include: Vec::new(),
    with: None,
    only: None,
    concurrently: None,
        schema: None,
        existence_guard: None,
        nulls_not_distinct: None,
    };
    let live = BTreeSet::from(["users".to_string()]);

    let pg = lower(vec![op.clone()], SqlDialect::Postgres, &live);
    assert_eq!(
        pg[0].up,
        r#"CREATE INDEX IF NOT EXISTS "users_email_lower_idx" ON "app"."users" ("email", (lower("email"))) WHERE ("active" IS TRUE)"#
    );

    let sqlite = lower(vec![op], SqlDialect::Sqlite, &live);
    assert_eq!(
        sqlite[0].up,
        r#"CREATE INDEX IF NOT EXISTS "users_email_lower_idx" ON "users" ("email", (lower("email"))) WHERE ("active" = 1)"#
    );
}

#[test]
fn mysql_fail_closes_on_expression_index_elements() {
    let err = validate_ir(
        &ir(vec![Op::CreateIndex {
                table: "users".into(),
                columns: vec![IndexElement::Expr { expr: lower_email_expr() }],
                name: Some("users_email_lower_idx".into()),
                unique: None,
                using: None,
                r#where: None,

            include: Vec::new(),
            with: None,
            only: None,
            concurrently: None,
                schema: None,
                existence_guard: None,
                nulls_not_distinct: None,
            }]),
        Dialect::Mysql,
        &[],
    )
        .unwrap_err();
    assert_eq!(err.code, CODE_UNSUPPORTED);
    assert_eq!(err.kind, Some(UnsupportedKind::Op));
    assert!(err.reason.contains("expression elements"));
}

#[test]
fn mysql_fail_closes_on_partial_index_predicate() {
    let err = validate_ir(
        &ir(vec![Op::CreateIndex {
                table: "users".into(),
                columns: vec![idx_col("active")],
                name: Some("users_active_idx".into()),
                unique: None,
                using: None,
                r#where: Some(active_true_expr()),

            include: Vec::new(),
            with: None,
            only: None,
            concurrently: None,
                schema: None,
                existence_guard: None,
                nulls_not_distinct: None,
            }]),
        Dialect::Mysql,
        &[],
    )
        .unwrap_err();
    assert_eq!(err.code, CODE_UNSUPPORTED);
    assert_eq!(err.kind, Some(UnsupportedKind::Op));
    assert!(err.reason.contains("partial indexes"));
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
    for dialect in [Dialect::Sqlite, Dialect::Mysql] {
        let err = validate_ir(
            &ir(vec![Op::AddConstraint {
                table: "bookings".into(),
                constraint: exclusion_constraint(),
                schema: None,
                existence_guard: None,
            }]),
            dialect,
            &[],
        )
        .unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Op));
        assert!(err.reason.contains("exclusion"));
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
