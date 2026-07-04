//! Partition-aware drift detection against live Postgres.
//!
//! Requires `zeroship_migrate_test` on :5440, matching the other `*_pg` tests.

use compio_postgres::Client;
use zeroship_migrate::{
    apply, diff_snapshots, fold_ops, snapshot_schema, Approval, ColType, ExecutorConfig, Expr,
    IndexElement, IndexMethod, IndexStorageParams, IrAuthor, IrColumn, IrFlagsOverride,
    LiveSchema, MigrationIr, Op, PartitionBoundValue, PartitionBounds, PartitionSpec,
    SchemaScope, SqlDialect, UnaryOp, CURRENT_IR_VERSION,
};

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";

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

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.pg.meta_schema = format!("meta_{tok}");
    c
}

async fn ensure_project_schema(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{}\"", cfg.project_schema))
        .await
        .expect("create project schema");
}

async fn drop_schemas(conn: &Client, cfg: &ExecutorConfig) {
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}

fn ir_col(name: &str, ty: ColType, nullable: bool) -> IrColumn {
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

fn idx_col(name: &str) -> IndexElement {
    IndexElement::Column {
        name: name.to_string(),
        order: None,
    }
}

fn ir_doc(name: &str, ops: Vec<Op>) -> MigrationIr {
    MigrationIr {
        ir_version: CURRENT_IR_VERSION,
        name: name.to_string(),
        owner_app: "app_test".to_string(),
        ops,
        flags: IrFlagsOverride::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    }
}

fn lower_ir_migrations(
    schema: &str,
    name: &str,
    ops: &[Op],
) -> Vec<zeroship_migrate::Migration> {
    let author = IrAuthor::new(schema, "app_test", SqlDialect::Postgres)
        .with_schema_scope(SchemaScope::Allowlist(vec![schema.to_string()]));
    author
        .lower(&ir_doc(name, ops.to_vec()), &LiveSchema::default())
        .expect("lower authored IR to migrations")
}

fn ts_bound(value: &str) -> PartitionBoundValue {
    PartitionBoundValue::String {
        value: value.to_string(),
    }
}

fn user_id_is_present() -> Expr {
    Expr::UnaryOp {
        op: UnaryOp::IsNotNull,
        operand: Box::new(Expr::ColRef {
            name: "user_id".to_string(),
        }),
    }
}

fn partition_ops() -> Vec<Op> {
    vec![
        Op::CreateTable {
            name: "events".to_string(),
            columns: vec![
                ir_col("ts", ColType::Timestamp, false),
                ir_col("sandbox_id", ColType::Text, false),
                ir_col("user_id", ColType::Text, true),
                ir_col("data", ColType::Json, false),
            ],
            primary_key: None,
            constraints: Vec::new(),
            indexes: Vec::new(),
            partition_by: Some(PartitionSpec::Range {
                columns: vec!["ts".to_string()],
            }),
            runtime_options: None,
            schema: None,
            existence_guard: None,
        },
        Op::CreatePartition {
            name: "events_2026_05".to_string(),
            of: "events".to_string(),
            bounds: PartitionBounds::Range {
                from: vec![ts_bound("2026-05-01 00:00:00+00")],
                to: vec![ts_bound("2026-06-01 00:00:00+00")],
            },
            schema: None,
            existence_guard: None,
        },
        Op::CreatePartition {
            name: "events_default".to_string(),
            of: "events".to_string(),
            bounds: PartitionBounds::Default,
            schema: None,
            existence_guard: None,
        },
        Op::CreateIndex {
            table: "events".to_string(),
            columns: vec![idx_col("ts")],
            name: Some("events_ts_brin_idx".to_string()),
            unique: None,
            using: Some(IndexMethod::Brin),
            r#where: None,
            include: Vec::new(),
            with: Some(IndexStorageParams {
                pages_per_range: Some(32),
                fillfactor: None,
            }),
            only: None,
            concurrently: None,
            schema: None,
            existence_guard: None,
        },
        Op::CreateIndex {
            table: "events".to_string(),
            columns: vec![idx_col("ts")],
            name: Some("events_ts_cover_idx".to_string()),
            unique: None,
            using: None,
            r#where: Some(user_id_is_present()),
            include: vec![
                "sandbox_id".to_string(),
                "user_id".to_string(),
                "data".to_string(),
            ],
            with: None,
            only: None,
            concurrently: None,
            schema: None,
            existence_guard: None,
        },
    ]
}

#[compio::test]
async fn partitioned_table_drift_round_trips_without_spurious_children_or_index_facets() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let ops = partition_ops();
    let migrations = lower_ir_migrations(&cfg.project_schema, "partition_drift", &ops);
    apply(&conn, &cfg, &migrations, Approval::None, "actor")
        .await
        .expect("apply partitioned table ops");

    let expected = fold_ops(&ops, SqlDialect::Postgres, &cfg.project_schema)
        .expect("fold partitioned table ops");
    let actual = snapshot_schema(&conn, &cfg.project_schema)
        .await
        .expect("introspect partitioned table");

    assert!(
        !actual.tables.contains_key("events_2026_05")
            && !actual.tables.contains_key("events_default"),
        "child partitions must not be modeled as independent tables: {actual:#?}"
    );
    assert!(
        actual.partitions.contains_key("events_2026_05")
            && actual.partitions.contains_key("events_default"),
        "child partitions must be modeled in SchemaSnapshot::partitions: {actual:#?}"
    );

    let drift = diff_snapshots(&expected, &actual);
    assert!(
        drift.is_clean(),
        "partitioned table fold and live catalog must agree without child-table or index-facet drift: {drift:#?}"
    );

    drop_schemas(&conn, &cfg).await;
}
