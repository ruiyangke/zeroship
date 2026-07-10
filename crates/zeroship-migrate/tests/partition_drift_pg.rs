#![cfg(feature = "native-pg")]
//! Partition-aware drift detection against live Postgres.
//!
//! Requires `zeroship_migrate_test` on :5440, matching the other `*_pg` tests.

use compio_postgres::Client;
use zeroship_migrate::{
    apply, diff_snapshots, fold_ops, snapshot_schema, Approval, ColType, ExecutorConfig, Expr,
    IndexElement, IndexMethod, IndexStorageParams, IrAuthor, IrColumn, IrFlagsOverride, IrScalar,
    IrValue, LiveSchema, LockMode, MigrationEngine, MigrationIr, Op, PartitionBoundValue,
    PartitionBounds, PartitionSpec, PolicyProfile, PostgresBackend, SafeI64, SchemaScope,
    SqlDialect, UnaryOp, CURRENT_IR_VERSION,
};
use zeroship_migrate::render::step::PlanStep;
use zeroship_migrate::model::validate::{validate_ir_scoped, Dialect};

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
        opclass: None,
        collation: None,
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

fn int_bound(value: i64) -> PartitionBoundValue {
    PartitionBoundValue::Int {
        value: SafeI64::new(value).expect("test partition bound is JS-safe"),
    }
}

fn create_events_parent() -> Op {
    Op::CreateTable {
        name: "events".to_string(),
        columns: vec![
            ir_col("bucket", ColType::Int, false),
            ir_col("payload", ColType::Text, false),
        ],
        primary_key: None,
        constraints: Vec::new(),
        indexes: Vec::new(),
        partition_by: Some(PartitionSpec::Range {
            columns: vec!["bucket".to_string()],
            collapse: true,
        }),
        runtime_options: None,
        schema: None,
        existence_guard: None,
    }
}

fn create_range_partition(name: &str, from: PartitionBoundValue, to: PartitionBoundValue) -> Op {
    Op::CreatePartition {
        name: name.to_string(),
        of: "events".to_string(),
        bounds: PartitionBounds::Range {
            from: vec![from],
            to: vec![to],
        },
        schema: None,
        existence_guard: None,
    }
}

fn create_default_partition() -> Op {
    Op::CreatePartition {
        name: "events_default".to_string(),
        of: "events".to_string(),
        bounds: PartitionBounds::Default,
        schema: None,
        existence_guard: None,
    }
}

fn drop_events_partition(name: &str) -> Op {
    Op::DropPartition {
        parent: "events".to_string(),
        name: name.to_string(),
        schema: None,
        existence_guard: None,
        cascade: None,
    }
}

fn insert_events(rows: &[(i64, &str)]) -> Op {
    Op::Insert {
        table: "events".to_string(),
        columns: vec!["bucket".to_string(), "payload".to_string()],
        rows: rows
            .iter()
            .map(|(bucket, payload)| {
                vec![
                    IrValue::from(IrScalar::Int(*bucket)),
                    IrValue::from(IrScalar::Str((*payload).to_string())),
                ]
            })
            .collect(),
        on_conflict: None,
        schema: None,
    }
}

fn live_from_fold(schema: &str, ops: &[Op]) -> LiveSchema {
    let snap = fold_ops(ops, SqlDialect::Postgres, schema).expect("fold partition ops");
    let mut live = LiveSchema::from_tables(snap.tables.keys().cloned().collect());
    live.table_snapshots = snap.tables;
    live.partitions = snap.partitions;
    live
}

fn lower_pg_steps(schema: &str, name: &str, ops: &[Op], live: &LiveSchema) -> Vec<PlanStep> {
    let author = IrAuthor::new(schema, "app_test", SqlDialect::Postgres)
        .with_schema_scope(SchemaScope::Allowlist(vec![schema.to_string()]));
    author
        .lower_steps(&ir_doc(name, ops.to_vec()), live)
        .expect("lower partition steps to Postgres")
}

async fn apply_pg_steps(
    conn: &Client,
    cfg: &ExecutorConfig,
    steps: &[PlanStep],
    approval: Approval,
) -> Result<zeroship_migrate::DeclarativeDeployOutcome, zeroship_migrate::DeclarativeApplyError> {
    MigrationEngine::new()
        .apply_plan(
            steps,
            approval,
            &PostgresBackend::new(conn),
            cfg,
            "partition-test",
            LockMode::Acquire,
        )
        .await
}

async fn pg_event_rows(conn: &Client, schema: &str) -> Vec<(i32, String)> {
    let rows = conn
        .query(
            &format!("SELECT bucket, payload FROM \"{schema}\".\"events\" ORDER BY bucket"),
            &[],
        )
        .await
        .expect("read events");
    rows.into_iter()
        .map(|row| (row.get::<_, i32>(0), row.get::<_, String>(1)))
        .collect()
}

fn user_id_is_present() -> Expr {
    Expr::UnaryOp {
        op: UnaryOp::IsNotNull,
        operand: Box::new(Expr::ColRef {
            name: "user_id".to_string(),
            table: None,
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
                collapse: false,
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
            nulls_not_distinct: None,
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
            nulls_not_distinct: None,
        },
    ]
}

fn collapse_events_ops() -> Vec<Op> {
    vec![
        create_events_parent(),
        create_range_partition("events_0", int_bound(0), int_bound(100)),
        create_default_partition(),
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

#[compio::test]
async fn collapse_affirmed_events_apply_natively_on_postgres() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let ops = collapse_events_ops();
    let ir = ir_doc("partition_collapse_pg", ops.clone());
    validate_ir_scoped(
        &ir,
        Dialect::Postgres,
        &[],
        None,
        &PolicyProfile::platform(),
    )
    .expect("collapse-affirmed partition recording validates on Postgres");
    let migrations = lower_ir_migrations(&cfg.project_schema, "partition_collapse_pg", &ops);
    let rendered = migrations
        .iter()
        .map(|m| m.up.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rendered.contains("PARTITION BY RANGE"),
        "Postgres leg must keep native partitioning:\n{rendered}"
    );
    assert!(
        rendered.contains("PARTITION OF"),
        "Postgres leg must render child partitions:\n{rendered}"
    );

    apply(&conn, &cfg, &migrations, Approval::None, "actor")
        .await
        .expect("apply collapse-affirmed partition ops on Postgres");
    conn.batch_execute(&format!(
        "INSERT INTO \"{}\".\"events\" (bucket, payload) VALUES (42, 'range'), (250, 'default')",
        cfg.project_schema
    ))
    .await
    .expect("insert through partitioned parent");

    let rows = conn
        .query(
            &format!(
                "SELECT bucket, payload, tableoid::regclass::text \
                   FROM \"{}\".\"events\" ORDER BY bucket",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("read partitioned parent rows");
    let got = rows
        .iter()
        .map(|row| {
            (
                row.get::<_, i32>(0),
                row.get::<_, String>(1),
                row.get::<_, String>(2),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(got.len(), 2, "both rows read through parent: {got:?}");
    assert_eq!((got[0].0, got[0].1.as_str()), (42, "range"));
    assert_eq!((got[1].0, got[1].1.as_str()), (250, "default"));
    assert!(
        got[0].2.ends_with("events_0") && got[1].2.ends_with("events_default"),
        "rows must physically land in native child partitions: {got:?}"
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn collapse_bounded_child_drop_matches_native_postgres_drop_table() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let ops = vec![
        create_events_parent(),
        create_range_partition("events_0", int_bound(0), int_bound(100)),
        create_default_partition(),
        insert_events(&[(42, "range"), (150, "default-a"), (250, "default-b")]),
        drop_events_partition("events_0"),
    ];
    let steps = lower_pg_steps(&cfg.project_schema, "partition_bounded_drop_pg", &ops, &LiveSchema::default());
    let rendered = format!("{steps:#?}");
    assert!(
        rendered.contains("DROP TABLE") && rendered.contains("events_0"),
        "Postgres bounded child drop must stay native DROP TABLE:\n{rendered}"
    );
    apply_pg_steps(&conn, &cfg, &steps, Approval::Approved)
        .await
        .expect("apply bounded child drop on Postgres");
    assert_eq!(
        pg_event_rows(&conn, &cfg.project_schema).await,
        vec![
            (150, "default-a".to_string()),
            (250, "default-b".to_string()),
        ]
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn collapse_default_child_drop_matches_native_postgres_drop_table() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let ops = vec![
        create_events_parent(),
        create_range_partition("events_0", int_bound(0), int_bound(100)),
        create_default_partition(),
        insert_events(&[(42, "range"), (150, "default-a"), (250, "default-b")]),
        drop_events_partition("events_default"),
    ];
    let steps = lower_pg_steps(&cfg.project_schema, "partition_default_drop_pg", &ops, &LiveSchema::default());
    apply_pg_steps(&conn, &cfg, &steps, Approval::Approved)
        .await
        .expect("apply default child drop on Postgres");
    assert_eq!(
        pg_event_rows(&conn, &cfg.project_schema).await,
        vec![(42, "range".to_string())]
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn create_bounded_child_mirrors_populated_default_error_on_postgres() {
    let conn = pg().await;

    let dirty_cfg = cfg_for(&token());
    drop_schemas(&conn, &dirty_cfg).await;
    ensure_project_schema(&conn, &dirty_cfg).await;
    let dirty_ops = vec![
        create_events_parent(),
        create_default_partition(),
        insert_events(&[(42, "stray")]),
        create_range_partition("events_0", int_bound(0), int_bound(100)),
    ];
    let dirty_steps = lower_pg_steps(
        &dirty_cfg.project_schema,
        "partition_mirror_dirty_pg",
        &dirty_ops,
        &LiveSchema::default(),
    );
    let err = apply_pg_steps(&conn, &dirty_cfg, &dirty_steps, Approval::None)
        .await
        .expect_err("native PG must reject creating a bounded child over matching default rows");
    assert!(
        format!("{err:?}").contains("updated partition constraint")
            || format!("{err:?}").contains("would be violated"),
        "unexpected PG populated-default error: {err:?}"
    );
    drop_schemas(&conn, &dirty_cfg).await;

    let clean_cfg = cfg_for(&token());
    drop_schemas(&conn, &clean_cfg).await;
    ensure_project_schema(&conn, &clean_cfg).await;
    let clean_ops = vec![
        create_events_parent(),
        create_default_partition(),
        insert_events(&[(250, "default")]),
        create_range_partition("events_0", int_bound(0), int_bound(100)),
    ];
    let clean_steps = lower_pg_steps(
        &clean_cfg.project_schema,
        "partition_mirror_clean_pg",
        &clean_ops,
        &LiveSchema::default(),
    );
    apply_pg_steps(&conn, &clean_cfg, &clean_steps, Approval::None)
        .await
        .expect("native PG must accept clean default when adding bounded child");
    assert_eq!(
        pg_event_rows(&conn, &clean_cfg.project_schema).await,
        vec![(250, "default".to_string())]
    );
    drop_schemas(&conn, &clean_cfg).await;
}

#[compio::test]
async fn child_create_down_round_trip_matches_native_postgres_end_states() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let up_ops = vec![
        create_events_parent(),
        create_range_partition("events_0", int_bound(0), int_bound(100)),
        create_default_partition(),
        insert_events(&[(42, "range"), (150, "default-a"), (250, "default-b")]),
    ];
    let up_steps = lower_pg_steps(&cfg.project_schema, "partition_up_pg", &up_ops, &LiveSchema::default());
    apply_pg_steps(&conn, &cfg, &up_steps, Approval::None)
        .await
        .expect("apply native partition up");
    assert_eq!(
        pg_event_rows(&conn, &cfg.project_schema).await,
        vec![
            (42, "range".to_string()),
            (150, "default-a".to_string()),
            (250, "default-b".to_string()),
        ]
    );

    let live = live_from_fold(&cfg.project_schema, &up_ops);
    let down_child_ops = vec![
        drop_events_partition("events_default"),
        drop_events_partition("events_0"),
    ];
    let down_child_steps = lower_pg_steps(
        &cfg.project_schema,
        "partition_down_children_pg",
        &down_child_ops,
        &live,
    );
    apply_pg_steps(&conn, &cfg, &down_child_steps, Approval::Approved)
        .await
        .expect("apply native semantic child drops");
    assert!(
        pg_event_rows(&conn, &cfg.project_schema).await.is_empty(),
        "child drops should remove all rows before parent drop"
    );

    let drop_parent_steps = lower_pg_steps(
        &cfg.project_schema,
        "partition_down_parent_pg",
        &[Op::DropTable {
            table: "events".to_string(),
            cascade: None,
            schema: None,
            existence_guard: None,
        }],
        &LiveSchema::default(),
    );
    apply_pg_steps(&conn, &cfg, &drop_parent_steps, Approval::Approved)
        .await
        .expect("apply native semantic down");
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.tables WHERE table_schema=$1 AND table_name='events'",
            &[&cfg.project_schema],
        )
        .await
        .expect("check events table absence");
    assert!(rows.is_empty(), "events table should be gone after down");

    drop_schemas(&conn, &cfg).await;
}
