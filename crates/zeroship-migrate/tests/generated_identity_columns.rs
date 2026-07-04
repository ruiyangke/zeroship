use zeroship_migrate::{
    fold_ops, validate_ir, BinaryOp, ColType, Expr, GeneratedCol, IdentityCol, IrColumn,
    IrConstraint, IrConstraintKind, IrDefault, IrFlagsOverride, IrLowerError, IrScalar, IrAuthor,
    LiveSchema, MigrationIr, Op, PolicyProfile, UnsupportedKind, ValidatorDialect,
    CODE_COLUMN_FACET_CONFLICT, CODE_UNSUPPORTED, CURRENT_IR_VERSION, resolve_create_table_policy,
    SchemaScope,
};
use zeroship_schema::query::SqlDialect;

const SCHEMA: &str = "app";
const OWNER: &str = "app_a";
type LowerResult = Result<String, Box<IrLowerError>>;
const SQLITE_SYSTEM_INDEXES: &str = r#";
CREATE INDEX IF NOT EXISTS "line_items_created_by_idx" ON "line_items" ("created_by");
CREATE INDEX IF NOT EXISTS "line_items_deleted_at_idx" ON "line_items" ("deleted_at");
CREATE INDEX IF NOT EXISTS "line_items_updated_at_idx" ON "line_items" ("updated_at")"#;

fn raw_ir(op: Op) -> MigrationIr {
    MigrationIr {
        ir_version: CURRENT_IR_VERSION,
        name: "generated_identity_columns".to_string(),
        owner_app: OWNER.to_string(),
        ops: vec![op],
        flags: IrFlagsOverride::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    }
}

fn ir(op: Op) -> MigrationIr {
    let ir = raw_ir(op);
    resolve_create_table_policy(&ir, &PolicyProfile::confined()).expect("test IR resolves")
}

fn validate_platform(ir: &MigrationIr, dialect: ValidatorDialect) -> Result<(), zeroship_migrate::AuthoringError> {
    zeroship_migrate::model::validate::validate_ir_scoped(
        ir,
        dialect,
        &[],
        Some(&SchemaScope::Unconfined),
        &PolicyProfile::platform(),
    )
}

fn col(name: &str, ty: ColType) -> IrColumn {
    IrColumn {
        name: name.to_string(),
        ty,
        nullable: Some(true),
        default: None,
        unique: None,
        id_prefix: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
    }
}

fn generated_total(stored: bool) -> GeneratedCol {
    GeneratedCol {
        expr: Expr::BinOp {
            op: BinaryOp::Mul,
            lhs: Box::new(Expr::col("qty")),
            rhs: Box::new(Expr::col("unit_cents")),
        },
        stored,
    }
}

fn create_table(columns: Vec<IrColumn>, constraints: Vec<IrConstraint>) -> Op {
    let mut primary_key = None;
    let constraints = constraints
        .into_iter()
        .filter_map(|constraint| match &constraint.kind {
            IrConstraintKind::Pk { columns } => {
                primary_key = Some(columns.clone());
                None
            }
            _ => Some(constraint),
        })
        .collect();
    Op::CreateTable {
        name: "line_items".to_string(),
        columns,
        primary_key,
        constraints,
        indexes: Vec::new(),

    partition_by: None,

    runtime_options: None,
            schema: None,
        existence_guard: None,
    }
}

fn pk_id() -> IrConstraint {
    IrConstraint {
        name: None,
        kind: IrConstraintKind::Pk {
            columns: vec!["id".to_string()],
        },
    }
}

fn lower_create(dialect: SqlDialect, op: Op) -> LowerResult {
    let author = IrAuthor::new(SCHEMA, OWNER, dialect);
    let migrations = author
        .lower(&ir(op), &LiveSchema::default())
        .map_err(Box::new)?;
    Ok(migrations
        .into_iter()
        .find(|m| m.up.starts_with("CREATE TABLE"))
        .expect("create table migration")
        .up)
}

fn lower_first(dialect: SqlDialect, op: Op) -> LowerResult {
    let author = IrAuthor::new(SCHEMA, OWNER, dialect);
    let migrations = author
        .lower(&ir(op), &LiveSchema::default())
        .map_err(Box::new)?;
    Ok(migrations.into_iter().next().expect("migration").up)
}

fn generated_create(stored: bool) -> Op {
    let mut total = col("total_cents", ColType::Int);
    total.generated = Some(generated_total(stored));
    total.nullable = Some(false);
    create_table(
        vec![
            col("qty", ColType::Int),
            total,
            col("unit_cents", ColType::Int),
        ],
        Vec::new(),
    )
}

#[test]
fn pg_generated_stored_column_renders_exact_create_table_ddl() {
    let up = lower_create(SqlDialect::Postgres, generated_create(true)).unwrap();
    assert_eq!(
        up,
        r#"CREATE TABLE "app"."line_items" ("created_at" timestamptz NOT NULL, "created_by" text, "deleted_at" timestamptz, "id" text PRIMARY KEY NOT NULL, "qty" integer, "total_cents" integer GENERATED ALWAYS AS (("qty" * "unit_cents")) STORED NOT NULL, "unit_cents" integer, "updated_at" timestamptz NOT NULL, "updated_by" text, "version" integer NOT NULL)"#,
    );
}

#[test]
fn sqlite_generated_stored_and_virtual_columns_render_exact_create_table_ddl() {
    let stored = lower_create(SqlDialect::Sqlite, generated_create(true)).unwrap();
    assert_eq!(
        stored,
        format!(
            r#"CREATE TABLE "line_items" ("created_at" TEXT NOT NULL, "created_by" TEXT, "deleted_at" TEXT, "id" TEXT PRIMARY KEY NOT NULL, "qty" INTEGER, "total_cents" INTEGER GENERATED ALWAYS AS (("qty" * "unit_cents")) STORED NOT NULL, "unit_cents" INTEGER, "updated_at" TEXT NOT NULL, "updated_by" TEXT, "version" INTEGER NOT NULL){SQLITE_SYSTEM_INDEXES}"#
        ),
    );

    let virtual_col = lower_create(SqlDialect::Sqlite, generated_create(false)).unwrap();
    assert_eq!(
        virtual_col,
        format!(
            r#"CREATE TABLE "line_items" ("created_at" TEXT NOT NULL, "created_by" TEXT, "deleted_at" TEXT, "id" TEXT PRIMARY KEY NOT NULL, "qty" INTEGER, "total_cents" INTEGER GENERATED ALWAYS AS (("qty" * "unit_cents")) VIRTUAL NOT NULL, "unit_cents" INTEGER, "updated_at" TEXT NOT NULL, "updated_by" TEXT, "version" INTEGER NOT NULL){SQLITE_SYSTEM_INDEXES}"#
        ),
    );
}

#[test]
fn pg_virtual_generated_column_is_unsupported() {
    let err = validate_ir(&ir(generated_create(false)), ValidatorDialect::Postgres, &[])
        .expect_err("Postgres supports generated columns only as STORED");
    assert_eq!(err.code, CODE_UNSUPPORTED, "got: {err}");
    assert_eq!(err.kind, Some(UnsupportedKind::VirtualColumn));
}

#[test]
fn generated_column_cannot_also_have_default() {
    let mut total = col("total_cents", ColType::Int);
    total.generated = Some(generated_total(true));
    total.default = Some(IrDefault::Literal {
        value: IrScalar::Int(0),
    });

    let err = validate_ir(
        &ir(create_table(vec![col("qty", ColType::Int), col("unit_cents", ColType::Int), total], Vec::new())),
        ValidatorDialect::Postgres,
        &[],
    )
    .expect_err("generated + default is a column-facet conflict");
    assert_eq!(err.code, CODE_COLUMN_FACET_CONFLICT, "got: {err}");
}

#[test]
fn pg_identity_always_and_by_default_render_exact_create_table_ddl() {
    let mut id = col("id", ColType::BigInt);
    id.nullable = Some(false);
    id.identity = Some(IdentityCol { always: true });
    let always = lower_create(SqlDialect::Postgres, create_table(vec![id], vec![pk_id()])).unwrap();
    assert_eq!(
        always,
        r#"CREATE TABLE "app"."line_items" ("created_at" timestamptz NOT NULL, "created_by" text, "deleted_at" timestamptz, "id" bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY NOT NULL, "updated_at" timestamptz NOT NULL, "updated_by" text, "version" integer NOT NULL)"#,
    );

    let mut id = col("id", ColType::BigInt);
    id.nullable = Some(false);
    id.identity = Some(IdentityCol { always: false });
    let by_default = lower_create(SqlDialect::Postgres, create_table(vec![id], vec![pk_id()])).unwrap();
    assert_eq!(
        by_default,
        r#"CREATE TABLE "app"."line_items" ("created_at" timestamptz NOT NULL, "created_by" text, "deleted_at" timestamptz, "id" bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY NOT NULL, "updated_at" timestamptz NOT NULL, "updated_by" text, "version" integer NOT NULL)"#,
    );
}

#[test]
fn sqlite_identity_only_emulates_sole_integer_primary_key() {
    let mut id = col("id", ColType::BigInt);
    id.nullable = Some(false);
    id.identity = Some(IdentityCol { always: true });
    let up = lower_create(SqlDialect::Sqlite, create_table(vec![id], vec![pk_id()])).unwrap();
    assert_eq!(
        up,
        format!(
            r#"CREATE TABLE "line_items" ("created_at" TEXT NOT NULL, "created_by" TEXT, "deleted_at" TEXT, "id" INTEGER PRIMARY KEY AUTOINCREMENT, "updated_at" TEXT NOT NULL, "updated_by" TEXT, "version" INTEGER NOT NULL){SQLITE_SYSTEM_INDEXES}"#
        ),
    );

    let mut seq = col("seq", ColType::Int);
    seq.identity = Some(IdentityCol { always: false });
    let err = validate_ir(
        &ir(create_table(vec![seq], Vec::new())),
        ValidatorDialect::Sqlite,
        &[],
    )
    .expect_err("SQLite non-PK identity has no sound emulation");
    assert_eq!(err.code, CODE_UNSUPPORTED, "got: {err}");
    assert_eq!(err.kind, Some(UnsupportedKind::Identity));
}

#[test]
fn identity_cannot_also_have_default_or_generated() {
    let mut id_with_default = col("id", ColType::BigInt);
    id_with_default.identity = Some(IdentityCol { always: true });
    id_with_default.default = Some(IrDefault::Literal {
        value: IrScalar::Int(1),
    });
    let err = validate_platform(
        &raw_ir(create_table(vec![id_with_default], vec![pk_id()])),
        ValidatorDialect::Postgres,
    )
    .expect_err("identity + default is a conflict");
    assert_eq!(err.code, CODE_COLUMN_FACET_CONFLICT, "got: {err}");

    let mut id_with_generated = col("id", ColType::BigInt);
    id_with_generated.identity = Some(IdentityCol { always: true });
    id_with_generated.generated = Some(GeneratedCol {
        expr: Expr::lit(IrScalar::Int(1)),
        stored: true,
    });
    let err = validate_platform(
        &raw_ir(create_table(vec![id_with_generated], vec![pk_id()])),
        ValidatorDialect::Postgres,
    )
    .expect_err("identity + generated is a conflict");
    assert_eq!(err.code, CODE_COLUMN_FACET_CONFLICT, "got: {err}");
}

#[test]
fn generated_and_identity_facets_render_on_add_column() {
    let add_generated = Op::AddColumn {
        table: "line_items".to_string(),
        column: "total_cents".to_string(),
        ty: ColType::Int,
        nullable: Some(false),
        default: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: Some(generated_total(true)),
        identity: None,
        schema: None,
        existence_guard: None,
    };
    let up = lower_first(SqlDialect::Postgres, add_generated).unwrap();
    assert_eq!(
        up,
        r#"ALTER TABLE "app"."line_items" ADD COLUMN "total_cents" integer GENERATED ALWAYS AS (("qty" * "unit_cents")) STORED NOT NULL"#,
    );

    let add_identity = Op::AddColumn {
        table: "line_items".to_string(),
        column: "seq".to_string(),
        ty: ColType::BigInt,
        nullable: Some(false),
        default: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: Some(IdentityCol { always: false }),
        schema: None,
        existence_guard: None,
    };
    let up = lower_first(SqlDialect::Postgres, add_identity).unwrap();
    assert_eq!(
        up,
        r#"ALTER TABLE "app"."line_items" ADD COLUMN "seq" bigint GENERATED BY DEFAULT AS IDENTITY NOT NULL"#,
    );
}

#[test]
fn fold_carries_generated_and_identity_column_facets() {
    let snap = fold_ops(
        &[generated_create(true)],
        SqlDialect::Postgres,
        SCHEMA,
    )
    .expect("fold generated column");
    let table = snap.tables.get("line_items").expect("line_items table");
    let total = table.columns.iter().find(|c| c.name == "total_cents").unwrap();
    assert_eq!(
        total.generated.as_ref().map(|g| (g.expr.as_str(), g.stored)),
        Some((r#"("qty" * "unit_cents")"#, true)),
    );

    let mut id = col("id", ColType::BigInt);
    id.identity = Some(IdentityCol { always: false });
    let snap = fold_ops(
        &[create_table(vec![id], vec![pk_id()])],
        SqlDialect::Postgres,
        SCHEMA,
    )
    .expect("fold identity column");
    let table = snap.tables.get("line_items").expect("line_items table");
    let id = table.columns.iter().find(|c| c.name == "id").unwrap();
    assert_eq!(id.identity, Some(IdentityCol { always: false }));
}
