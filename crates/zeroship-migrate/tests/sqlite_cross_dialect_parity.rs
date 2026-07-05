//! Cross-dialect op.* parity fixture.
//!
//! This is deliberately creator-shaped rather than platform-shaped: a small app
//! schema with identity primary keys, enum/domain columns, generated columns,
//! indexes, and a structured view applies on both Postgres and SQLite through
//! the real IR lower + engine apply paths. The two live databases are
//! introspected and collapsed to the documented logical contract:
//!
//! - Postgres enum/domain objects are equivalent to SQLite inline CHECKs.
//! - Postgres identity columns are equivalent to SQLite INTEGER PRIMARY KEY
//!   AUTOINCREMENT for the sole integer primary-key case.
//! - Generated columns, indexes, and structured views survive both applies with
//!   the same logical shape.
//!
//! Dialect-only facets are asserted as boundaries, not hidden:
//! SQLite Body triggers are created and introspected on SQLite, while the same
//! Body trigger fails closed on Postgres (`triggerBody`). PostgreSQL-only
//! sequence/exclusion constructs fail closed on SQLite. Table-level CHECK
//! constraints are pinned as PostgreSQL-only in Slice A: they validate on PG but
//! still validate-refuse on SQLite/MySQL until those CHECK renderers land.
//! SQLite table-level FK/UNIQUE constraints validate-refuse until the SQLite
//! CREATE emitter threads those table constraints into its descriptor path.

use std::collections::BTreeSet;
use std::path::PathBuf;

use compio_postgres::Client;
use tempfile::TempDir;
use zeroship_migrate::model::ir::{
    ForEach, RaiseLevel, SelectAst, SelectItem, TableRef, TriggerAction, TriggerEvent,
    TriggerStmt, TriggerTiming, ViewQuery,
};
use zeroship_migrate::model::validate::{validate_ir, Dialect, UnsupportedKind, CODE_UNSUPPORTED};
use zeroship_migrate::{
    provision_migrator, apply::role::deprovision_migrator, Approval, BinaryOp, ColType, ColumnOrExpr,
    ExclusionElement, ExclusionMethod, ExclusionOperator, Expr, GeneratedCol, GuardConfig,
    IdentityCol, IndexElement, IrColumn, IrConstraint, IrConstraintKind, IrDefault,
    IrFlagsOverride, IrIndex, IrLowerError, IrScalar, IrAuthor, LiveSchema, MigrationEngine,
    MigrationIr, Op, PolicyProfile, RefAction, SafeI64, SequenceOwnedBy, SqliteBackend,
    CURRENT_IR_VERSION, resolve_create_table_policy,
};
use zeroship_migrate::{ExecutorConfig, SchemaScope};
use zeroship_schema::query::SqlDialect;

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";
const PG_SCHEMA: &str = "parity_app";
const PROJECT: &str = "parity_app";
const OWNER: &str = "app_parity";

fn dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| DEFAULT_DSN.to_string())
}

async fn pg() -> Client {
    let (client, conn) = compio_postgres::connect(&dsn(), compio_postgres::NoTls)
        .await
        .expect("connect to zeroship_migrate_test on :5440");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid}_{nanos}_{n}")
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn pg_cfg(tok: &str) -> ExecutorConfig {
    let mut cfg = ExecutorConfig::new(format!("prj_{tok}"), format!("{PG_SCHEMA}_{tok}"));
    cfg.pg.meta_schema = format!("meta_{tok}");
    let role = zeroship_migrate::migrator_role_name(&cfg.project_id).unwrap();
    cfg.with_migrator_role(role)
}

async fn setup_pg(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS {};",
        quote_ident(&cfg.project_schema)
    ))
    .await
    .expect("create parity schema");
    provision_migrator(conn, cfg)
        .await
        .expect("provision parity migrator role");
}

async fn teardown_pg(conn: &Client, cfg: &ExecutorConfig) {
    let _ = deprovision_migrator(conn, cfg).await;
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE; DROP SCHEMA IF EXISTS {} CASCADE;",
            quote_ident(&cfg.project_schema),
            quote_ident(&cfg.pg.meta_schema)
        ))
        .await;
}

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn sqlite_paths(tag: &str) -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join(format!("zs-{tag}.sqlite"));
    let journal = dir.path().join(format!("zs-{tag}.migrations.sqlite"));
    Paths {
        _dir: dir,
        app,
        journal,
    }
}

fn sqlite_backend(paths: &Paths) -> SqliteBackend {
    SqliteBackend::open(&paths.app, &paths.journal).expect("open sqlite parity backend")
}

fn ir(name: &str, ops: Vec<Op>) -> MigrationIr {
    let ir = MigrationIr {
        ir_version: CURRENT_IR_VERSION,
        name: name.to_string(),
        owner_app: OWNER.to_string(),
        ops,
        flags: IrFlagsOverride::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    };
    resolve_create_table_policy(&ir, &PolicyProfile::confined()).expect("test IR resolves")
}

fn col(name: &str, ty: ColType, nullable: bool) -> IrColumn {
    IrColumn {
        name: name.to_string(),
        ty,
        nullable: Some(nullable),
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

fn identity_id() -> IrColumn {
    let mut id = col("id", ColType::BigInt, false);
    id.identity = Some(IdentityCol { always: false });
    id
}

fn pk_id() -> IrConstraint {
    IrConstraint {
        name: None,
        kind: IrConstraintKind::Pk {
            columns: vec!["id".to_string()],
        },
    }
}

fn positive_value_check() -> Expr {
    Expr::BinOp {
        op: BinaryOp::Ge,
        lhs: Box::new(Expr::col("VALUE")),
        rhs: Box::new(Expr::lit(IrScalar::Int(0))),
    }
}

fn common_ops() -> Vec<Op> {
    let mut account_status = col(
        "status",
        ColType::Enum {
            name: "app_status".to_string(),
            schema: None,
        },
        false,
    );
    account_status.default = Some(IrDefault::Literal {
        value: IrScalar::Str("active".to_string()),
    });

    let mut order_status = account_status.clone();
    order_status.name = "status".to_string();

    let mut total = col("total_cents", ColType::Int, false);
    total.generated = Some(GeneratedCol {
        expr: Expr::BinOp {
            op: BinaryOp::Mul,
            lhs: Box::new(Expr::col("qty")),
            rhs: Box::new(Expr::col("unit_cents")),
        },
        stored: true,
    });

    vec![
        Op::CreateEnum {
            name: "app_status".to_string(),
            schema: None,
            values: vec!["active".to_string(), "paused".to_string()],
        },
        Op::CreateDomain {
            name: "positive_cents".to_string(),
            schema: None,
            as_type: ColType::Int,
            check: Some(positive_value_check()),
            default: Some(IrDefault::Literal {
                value: IrScalar::Int(0),
            }),
            not_null: Some(true),
        },
        Op::CreateTable {
            name: "accounts".to_string(),
            schema: None,
            columns: vec![
                identity_id(),
                col("name", ColType::Text, false),
                account_status,
                col(
                    "credit_cents",
                    ColType::Domain {
                        name: "positive_cents".to_string(),
                        schema: None,
                    },
                    false,
                ),
            ],
            primary_key: None,
            constraints: vec![pk_id()],
            indexes: Vec::new(),

        partition_by: None,

        runtime_options: None,
            existence_guard: None,
        },
        Op::CreateTable {
            name: "orders".to_string(),
            schema: None,
            columns: vec![
                identity_id(),
                col("account_id", ColType::BigInt, false),
                col("qty", ColType::Int, false),
                col(
                    "unit_cents",
                    ColType::Domain {
                        name: "positive_cents".to_string(),
                        schema: None,
                    },
                    false,
                ),
                total,
                order_status,
            ],
            primary_key: None,
            constraints: vec![pk_id()],
            indexes: vec![IrIndex {
                name: Some("orders_account_status_active_idx".to_string()),
                columns: vec![
                    IndexElement::Column {
                        name: "account_id".to_string(),
                        order: None,
                    },
                    IndexElement::Column {
                        name: "status".to_string(),
                        order: None,
                    },
                ],
                unique: Some(false),
                using: None,
                r#where: None,
            include: Vec::new(),
            with: None,
            only: None,
            }],

        partition_by: None,

        runtime_options: Default::default(),
            existence_guard: None,
        },
        Op::CreateView {
            name: "active_orders".to_string(),
            schema: None,
            columns: None,
            query: ViewQuery::Structured {
                select: SelectAst {
                    from: TableRef {
                        name: "orders".to_string(),
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
                            name: "account_id".to_string(),
                            alias: None,
                        },
                        SelectItem::ColRef {
                            table: None,
                            name: "total_cents".to_string(),
                            alias: None,
                        },
                    ],
                    joins: Vec::new(),
                    r#where: Some(Expr::UnaryOp {
                        op: zeroship_migrate::UnaryOp::IsNull,
                        operand: Box::new(Expr::col("deleted_at")),
                    }),
                    order_by: None,
                    limit: None,
                },
            },
            replace: None,
            materialized: None,
        },
    ]
}

fn sqlite_body_trigger_op() -> Op {
    Op::CreateTrigger {
        name: "orders_block_update".to_string(),
        table: "orders".to_string(),
        schema: None,
        timing: TriggerTiming::Before,
        events: vec![TriggerEvent::Update],
        for_each: ForEach::Row,
        action: TriggerAction::Body {
            statements: vec![TriggerStmt::Raise {
                level: RaiseLevel::Abort,
                message: "orders are append-only in this fixture".to_string(),
                errcode: None,
            }],
        },
        when: None,
    }
}

fn expected_core_fingerprint() -> BTreeSet<String> {
    [
        "table|accounts",
        "table|orders",
        "view|active_orders",
        "column|accounts.id|identity|required",
        "column|accounts.name|text|required",
        "column|accounts.status|enum:app_status|required",
        "column|accounts.credit_cents|domain:positive_cents|required",
        "column|orders.id|identity|required",
        "column|orders.account_id|bigint|required",
        "column|orders.qty|integer|required",
        "column|orders.unit_cents|domain:positive_cents|required",
        "column|orders.total_cents|integer|required|generated",
        "column|orders.status|enum:app_status|required",
        "pk|accounts|id",
        "pk|orders|id",
        "index|orders|orders_account_status_active_idx|account_id,status|plain",
        "enum|app_status|active,paused",
        "domain|positive_cents|integer|nonnegative|notnull|default=0",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

async fn pg_query_lines(
    conn: &Client,
    sql: &str,
    params: &[&(dyn compio_postgres::types::ToSql + Sync)],
) -> Vec<String> {
    let rows = conn.query(sql, params).await.expect("pg parity query");
    rows.iter().map(|r| r.get::<_, String>(0)).collect()
}

async fn pg_core_fingerprint(conn: &Client, schema: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    out.extend(pg_query_lines(
        conn,
        "SELECT concat(CASE WHEN c.relkind = 'v' THEN 'view' ELSE 'table' END, '|', c.relname) \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relname IN ('accounts', 'orders', 'active_orders') \
           AND c.relkind IN ('r', 'v')",
        &[&schema],
    ).await);
    out.extend(pg_query_lines(
        conn,
        "SELECT concat('column|', c.relname, '.', a.attname, '|',
             CASE
               WHEN a.attname = 'id' THEN 'identity'
               WHEN a.attname = 'status' THEN 'enum:app_status'
               WHEN a.attname IN ('credit_cents', 'unit_cents') THEN 'domain:positive_cents'
               WHEN format_type(a.atttypid, a.atttypmod) = 'text' THEN 'text'
               WHEN format_type(a.atttypid, a.atttypmod) = 'integer' THEN 'integer'
               WHEN format_type(a.atttypid, a.atttypmod) = 'bigint' THEN 'bigint'
               ELSE format_type(a.atttypid, a.atttypmod)
             END,
             '|', CASE WHEN a.attnotnull THEN 'required' ELSE 'nullable' END,
             CASE WHEN a.attgenerated <> '' THEN '|generated' ELSE '' END
         )
         FROM pg_attribute a
         JOIN pg_class c ON c.oid = a.attrelid
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = $1
           AND c.relname IN ('accounts', 'orders')
           AND a.attname IN ('id', 'name', 'status', 'credit_cents', 'account_id', 'qty', 'unit_cents', 'total_cents')
           AND NOT a.attisdropped",
        &[&schema],
    ).await);
    out.extend(pg_query_lines(
        conn,
        "SELECT concat(
             CASE con.contype
               WHEN 'p' THEN 'pk'
               WHEN 'u' THEN 'unique'
               WHEN 'c' THEN 'check'
               WHEN 'f' THEN 'fk'
             END,
             '|', c.relname,
             CASE WHEN con.contype = 'p' THEN '|id' ELSE concat('|', con.conname) END,
             CASE WHEN con.contype = 'u' THEN '|name' ELSE '' END,
             CASE WHEN con.contype = 'f' THEN '|account_id|accounts|id|delete=cascade|update=restrict' ELSE '' END
         )
         FROM pg_constraint con
         JOIN pg_class c ON c.oid = con.conrelid
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = $1
           AND con.conname IN (
             'accounts_pkey',
             'orders_pkey'
           )",
        &[&schema],
    ).await);
    out.extend(pg_query_lines(
        conn,
        "SELECT concat('index|orders|', ic.relname, '|account_id,status|plain')
         FROM pg_index i
         JOIN pg_class ic ON ic.oid = i.indexrelid
         JOIN pg_class tc ON tc.oid = i.indrelid
         JOIN pg_namespace n ON n.oid = tc.relnamespace
         WHERE n.nspname = $1
           AND tc.relname = 'orders'
           AND ic.relname = 'orders_account_status_active_idx'
           AND i.indpred IS NULL",
        &[&schema],
    ).await);
    out.extend(pg_query_lines(
        conn,
        "SELECT concat('enum|app_status|', string_agg(e.enumlabel, ',' ORDER BY e.enumsortorder))
         FROM pg_type t
         JOIN pg_namespace n ON n.oid = t.typnamespace
         JOIN pg_enum e ON e.enumtypid = t.oid
         WHERE n.nspname = $1 AND t.typname = 'app_status'
         GROUP BY t.typname",
        &[&schema],
    ).await);
    out.extend(pg_query_lines(
        conn,
        "SELECT 'domain|positive_cents|integer|nonnegative|notnull|default=0'
         FROM pg_type t
         JOIN pg_namespace n ON n.oid = t.typnamespace
         JOIN pg_constraint con ON con.contypid = t.oid
         WHERE n.nspname = $1
           AND t.typname = 'positive_cents'
           AND t.typnotnull
           AND t.typdefault = '0'
           AND pg_get_constraintdef(con.oid, true) LIKE '%VALUE >= 0%'",
        &[&schema],
    ).await);
    out
}

async fn sqlite_rows(be: &SqliteBackend, sql: &str) -> Vec<Vec<Option<String>>> {
    be.actor()
        .set_mode(zeroship_migrate::apply::backend::sqlite::Mode::EngineJournal)
        .await
        .expect("engine mode");
    be.actor().query(sql).await.expect("sqlite parity query")
}

async fn sqlite_table_sql(be: &SqliteBackend, name: &str) -> String {
    let rows = sqlite_rows(
        be,
        &format!("SELECT sql FROM sqlite_master WHERE name = '{name}'"),
    )
    .await;
    rows[0][0].as_ref().expect("sqlite sql").clone()
}

async fn sqlite_core_fingerprint(be: &SqliteBackend) -> BTreeSet<String> {
    let mut out = BTreeSet::new();

    for name in ["accounts", "orders"] {
        let rows = sqlite_rows(
            be,
            &format!("SELECT name FROM sqlite_master WHERE type = 'table' AND name = '{name}'"),
        )
        .await;
        assert_eq!(rows.len(), 1, "SQLite table {name} exists");
        out.insert(format!("table|{name}"));
    }
    let view = sqlite_rows(
        be,
        "SELECT name FROM sqlite_master WHERE type = 'view' AND name = 'active_orders'",
    )
    .await;
    assert_eq!(view.len(), 1, "SQLite structured view exists");
    out.insert("view|active_orders".to_string());

    for table in ["accounts", "orders"] {
        let table_sql = sqlite_table_sql(be, table).await;
        let xinfo = sqlite_rows(be, &format!("PRAGMA main.table_info({table})")).await;
        for row in xinfo {
            let name = row[1].as_deref().unwrap_or_default();
            if ![
                "id",
                "name",
                "status",
                "credit_cents",
                "account_id",
                "qty",
                "unit_cents",
                "total_cents",
            ]
            .contains(&name)
            {
                continue;
            }
            let ty = match name {
                "id" => "identity".to_string(),
                "account_id" => "bigint".to_string(),
                "status" => "enum:app_status".to_string(),
                "credit_cents" | "unit_cents" => "domain:positive_cents".to_string(),
                _ => row[2].as_deref().unwrap_or_default().to_ascii_lowercase(),
            };
            let required = if row[3].as_deref() == Some("1") || name == "id" {
                "required"
            } else {
                "nullable"
            };
            out.insert(format!("column|{table}.{name}|{ty}|{required}"));
        }

        if table == "accounts" {
            assert!(
                table_sql.contains(r#"CHECK (("credit_cents" >= 0))"#)
                    && table_sql.contains(r#""status" TEXT NOT NULL DEFAULT 'active' CHECK ("status" IN ('active', 'paused'))"#)
                    && table_sql.contains(r#""credit_cents" INTEGER NOT NULL DEFAULT 0 CHECK (("credit_cents" >= 0))"#),
                "SQLite accounts table must inline enum/domain/check constraints: {table_sql}"
            );
            out.insert("pk|accounts|id".to_string());
            out.insert("enum|app_status|active,paused".to_string());
            out.insert("domain|positive_cents|integer|nonnegative|notnull|default=0".to_string());
        }
        if table == "orders" {
            assert!(
                table_sql.contains(r#"GENERATED ALWAYS AS (("qty" * "unit_cents")) STORED"#)
                    && table_sql.contains(r#""unit_cents" INTEGER NOT NULL DEFAULT 0 CHECK (("unit_cents" >= 0))"#),
                "SQLite orders table must inline generated/domain constraints: {table_sql}"
            );
            out.insert("column|orders.total_cents|integer|required|generated".to_string());
            out.insert("pk|orders|id".to_string());
        }
    }

    let index_sql = sqlite_rows(
        be,
        "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'orders_account_status_active_idx'",
    )
    .await;
    assert_eq!(index_sql.len(), 1, "SQLite partial index exists");
    let sql = index_sql[0][0].as_ref().expect("index sql");
    assert!(
        sql.contains(r#""account_id", "status""#) && !sql.contains(" WHERE "),
        "SQLite index must preserve columns and stay plain: {sql}"
    );
    out.insert("index|orders|orders_account_status_active_idx|account_id,status|plain".to_string());

    out
}

fn sqlite_pg_only_facets_fail_closed() {
    let sequence_err = IrAuthor::new(PROJECT, OWNER, SqlDialect::Sqlite)
        .lower(
            &ir(
                "sqlite_refuses_sequence",
                vec![Op::CreateSequence {
                    name: "invoice_seq".to_string(),
                    schema: None,
                    as_type: Some(ColType::BigInt),
                    increment: Some(SafeI64::new(1).unwrap()),
                    start: Some(SafeI64::new(1).unwrap()),
                    min_value: None,
                    max_value: None,
                    cache: None,
                    cycle: None,
                    owned_by: Some(Some(SequenceOwnedBy {
                        table: "orders".to_string(),
                        column: "id".to_string(),
                    })),
                }],
            ),
            &LiveSchema::default(),
        )
        .expect_err("SQLite must fail closed on sequences");
    assert!(matches!(
        sequence_err,
        IrLowerError::SequenceUnsupported {
            kind: "sequence",
            dialect: SqlDialect::Sqlite
        }
    ));

    let exclusion_err = IrAuthor::new(PROJECT, OWNER, SqlDialect::Sqlite)
        .lower(
            &ir(
                "sqlite_refuses_exclusion",
                vec![Op::AddConstraint {
                    table: "orders".to_string(),
                    schema: None,
                    constraint: IrConstraint {
                        name: Some("orders_no_duplicate_account_status".to_string()),
                        kind: IrConstraintKind::Exclusion {
                            using_method: ExclusionMethod::Gist,
                            elements: vec![ExclusionElement {
                                target: ColumnOrExpr::Column {
                                    name: "account_id".to_string(),
                                },
                                operator: ExclusionOperator::Equal,
                            }],
                            where_predicate: None,
                            deferrable: None,
                            initially_deferred: None,
                        },
                    },
                    existence_guard: None,
                }],
            ),
            &LiveSchema::from(&BTreeSet::from(["orders".to_string()])),
        )
        .expect_err("SQLite must fail closed on exclusion constraints");
    assert!(matches!(
        exclusion_err,
        IrLowerError::ExclusionConstraintUnsupported {
            kind: "exclusionConstraint",
            dialect: SqlDialect::Sqlite
        }
    ));
}

fn table_check_constraints_are_pg_only_until_non_pg_renderers_land() {
    let check_constraint = IrConstraint {
        name: Some("check_probe_qty_positive".to_string()),
        kind: IrConstraintKind::Check {
            expr: Expr::BinOp {
                op: BinaryOp::Ge,
                lhs: Box::new(Expr::col("qty")),
                rhs: Box::new(Expr::lit(IrScalar::Int(1))),
            },
        
            not_valid: None,
        },
    };
    let check_op = Op::CreateTable {
        name: "check_probe".to_string(),
        schema: None,
        columns: vec![col("qty", ColType::Int, false)],
        primary_key: None,
        constraints: vec![check_constraint.clone()],
        indexes: Vec::new(),

    partition_by: None,

    runtime_options: None,
            existence_guard: None,
    };
    let add_check_op = Op::AddConstraint {
        table: "check_probe".to_string(),
        schema: None,
        constraint: check_constraint,
        existence_guard: None,
    };

    validate_ir(&ir("table_check_pg", vec![check_op.clone()]), Dialect::Postgres, &[])
        .expect("PG table CHECK constraints validate after Slice A");
    validate_ir(
        &ir("add_table_check_pg", vec![add_check_op.clone()]),
        Dialect::Postgres,
        &[],
    )
    .expect("PG addConstraint(check) validates after Slice A");

    for dialect in [Dialect::Sqlite, Dialect::Mysql] {
        let err = validate_ir(&ir("table_check_gap", vec![check_op.clone()]), dialect, &[])
            .expect_err("non-PG table CHECK constraints are still validate-refused");
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        assert!(err.reason.contains("CHECK"));

        let err = validate_ir(
            &ir("add_table_check_gap", vec![add_check_op.clone()]),
            dialect,
            &[],
        )
        .expect_err("non-PG addConstraint(check) is still validate-refused");
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        assert!(err.reason.contains("CHECK") || err.reason.contains("check"));
    }
}

fn sqlite_table_fk_and_unique_constraints_fail_closed_until_emitter_threads_them() {
    let fk_op = Op::CreateTable {
        name: "orders".to_string(),
        schema: None,
        columns: vec![identity_id(), col("account_id", ColType::BigInt, false)],
        primary_key: None,
        constraints: vec![IrConstraint {
                name: Some("orders_account_fk".to_string()),
                kind: IrConstraintKind::Fk {
                    columns: vec!["account_id".to_string()],
                    references_table: "accounts".to_string(),
                    references_columns: vec!["id".to_string()],
                    on_delete: Some(RefAction::Cascade),
                    on_update: Some(RefAction::Restrict),
                    deferrable: None,
                    initially_deferred: None,
                
                    not_valid: None,
                },
            }],
        indexes: Vec::new(),

    partition_by: None,

    runtime_options: None,
            existence_guard: None,
    };
    let fk_err = validate_ir(&ir("sqlite_fk_gap", vec![fk_op]), Dialect::Sqlite, &[])
        .expect_err("SQLite table-level FK is currently validate-refused");
    assert_eq!(fk_err.code, CODE_UNSUPPORTED);
    assert_eq!(fk_err.kind, Some(UnsupportedKind::Op));
    assert!(fk_err.reason.contains("foreign keys"));

    let unique_op = Op::CreateTable {
        name: "accounts".to_string(),
        schema: None,
        columns: vec![identity_id(), col("name", ColType::Text, false)],
        primary_key: None,
        constraints: vec![IrConstraint {
                name: Some("accounts_name_unique".to_string()),
                kind: IrConstraintKind::Unique {
                    columns: vec!["name".to_string()],
                },
            }],
        indexes: Vec::new(),

    partition_by: None,

    runtime_options: None,
            existence_guard: None,
    };
    let unique_err = validate_ir(&ir("sqlite_unique_gap", vec![unique_op]), Dialect::Sqlite, &[])
        .expect_err("SQLite table-level UNIQUE is currently validate-refused");
    assert_eq!(unique_err.code, CODE_UNSUPPORTED);
    assert_eq!(unique_err.kind, Some(UnsupportedKind::Op));
    assert!(unique_err.reason.contains("unique"));
}

#[compio::test]
async fn op_ir_common_schema_is_equivalent_on_postgres_and_sqlite() {
    sqlite_pg_only_facets_fail_closed();
    table_check_constraints_are_pg_only_until_non_pg_renderers_land();
    sqlite_table_fk_and_unique_constraints_fail_closed_until_emitter_threads_them();

    let pg_body_err = IrAuthor::new(PG_SCHEMA, OWNER, SqlDialect::Postgres)
        .lower(
            &ir("pg_refuses_sqlite_body_trigger", vec![sqlite_body_trigger_op()]),
            &LiveSchema::from(&BTreeSet::from(["orders".to_string()])),
        )
        .expect_err("Postgres must fail closed on SQLite Body triggers");
    assert!(matches!(
        pg_body_err,
        IrLowerError::TriggerUnsupported {
            kind: "triggerBody",
            dialect: SqlDialect::Postgres
        }
    ));

    let conn = pg().await;
    let cfg = pg_cfg(&token());
    setup_pg(&conn, &cfg).await;

    let pg_author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres)
        .with_schema_scope(SchemaScope::Single(cfg.project_schema.clone()));
    let pg_migrations = pg_author
        .lower(&ir("cross_dialect_core", common_ops()), &LiveSchema::default())
        .expect("common IR lowers on Postgres");
    let pg_engine = MigrationEngine::new();
    let pg_plan = pg_engine.plan(&pg_migrations, &GuardConfig::confined(cfg.project_schema.clone()));
    assert!(pg_plan.denied.is_empty(), "PG parity plan denials: {:?}", pg_plan.denied);
    pg_engine
        .apply(
            &pg_plan,
            Approval::None,
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            OWNER,
        )
        .await
        .expect("common IR applies on Postgres");
    let pg_fp = pg_core_fingerprint(&conn, &cfg.project_schema).await;

    let paths = sqlite_paths("cross_dialect");
    let sqlite = sqlite_backend(&paths);
    let mut sqlite_ops = common_ops();
    sqlite_ops.push(sqlite_body_trigger_op());
    let sqlite_author = IrAuthor::new(PROJECT, OWNER, SqlDialect::Sqlite);
    let sqlite_migrations = sqlite_author
        .lower(&ir("cross_dialect_sqlite", sqlite_ops), &LiveSchema::default())
        .expect("common IR + Body trigger lowers on SQLite");
    for migration in &sqlite_migrations {
        sqlite
            .apply_one_additive(migration, OWNER)
            .await
            .unwrap_or_else(|e| panic!("SQLite applies {}: {e:?}", migration.name));
    }
    let sqlite_fp = sqlite_core_fingerprint(&sqlite).await;

    let expected = expected_core_fingerprint();
    assert_eq!(pg_fp, expected, "Postgres normalized parity fingerprint");
    assert_eq!(sqlite_fp, expected, "SQLite normalized parity fingerprint");

    let triggers = sqlite_rows(
        &sqlite,
        "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = 'orders_block_update'",
    )
    .await;
    assert_eq!(triggers.len(), 1, "SQLite Body trigger exists");
    assert!(
        triggers[0][0]
            .as_ref()
            .expect("trigger sql")
            .contains("RAISE(ABORT,'orders are append-only in this fixture')"),
        "SQLite trigger body is preserved"
    );

    teardown_pg(&conn, &cfg).await;
}
