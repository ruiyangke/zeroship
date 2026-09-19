use crate::support;

use zeroship_migrate::model::ir::{
    ColType, IndexElement, IndexMethod, IrColumn, IrScalar, IrValue, MigrationIr, Op,
    PartitionBoundValue, PartitionBounds, PartitionSpec, SafeI64,
};
use zeroship_migrate::{
    fold_ops, Approval, ExecutorConfig, IrAuthor, IrFlagsOverride, LiveSchema, LockMode,
    MigrationEngine, CURRENT_IR_VERSION,
};
use zeroship_migrate_sqlite::SqliteBackend;

fn col(name: &str, ty: ColType) -> IrColumn {
    col_with_nullability(name, ty, None)
}

fn not_null_col(name: &str, ty: ColType) -> IrColumn {
    col_with_nullability(name, ty, Some(false))
}

fn col_with_nullability(name: &str, ty: ColType, nullable: Option<bool>) -> IrColumn {
    IrColumn {
        name: name.into(),
        ty,
        nullable,
        default: None,
        unique: None,
        references: None,
        id_prefix: None,
        collation: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        encrypted: None,
        generated: None,
        identity: None,
    }
}

fn idx_col(name: &str) -> IndexElement {
    IndexElement::Column {
        name: name.into(),
        order: None,
        opclass: None,
        collation: None,
    }
}

fn ir(op: Op) -> MigrationIr {
    ir_ops(vec![op])
}

fn ir_ops(ops: Vec<Op>) -> MigrationIr {
    MigrationIr {
        inverse_ops: None,
        irreversible: None,
        ir_version: CURRENT_IR_VERSION,
        name: "partition_render".into(),
        owner_app: "app_partition".into(),
        ops,
        flags: IrFlagsOverride::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    }
}

fn pg_sql(op: Op) -> Vec<String> {
    IrAuthor::new(
        zeroship_migrate::shipping_vendors(),
        "app",
        "app_partition",
        &zeroship_migrate_postgres::DIALECT,
        &support::no_inject("app"),
    )
    .lower(&ir(op), &LiveSchema::default())
    .expect("lower")
    .into_iter()
    .map(|m| m.up)
    .collect()
}

/// Lower one op the way [`pg_sql`] does, but keep the whole `Migration` so a test can
/// read the FLAGS the author attached rather than only the SQL it spelled.
fn pg_lower(op: Op) -> Vec<zeroship_migrate::Migration> {
    IrAuthor::new(
        zeroship_migrate::shipping_vendors(),
        "app",
        "app_partition",
        &zeroship_migrate_postgres::DIALECT,
        &support::no_inject("app"),
    )
    .lower(&ir(op), &LiveSchema::default())
    .expect("lower")
}

fn int_bound(value: i64) -> PartitionBoundValue {
    PartitionBoundValue::Int {
        value: SafeI64::new(value).expect("test partition bound is JS-safe"),
    }
}

fn create_events_parent() -> Op {
    Op::CreateTable {
        attributes: zeroship_migrate_ir::attribute::CreateTableAttributes::new(),
        name: "events".into(),
        columns: vec![
            not_null_col("bucket", ColType::Int),
            not_null_col("payload", ColType::Text),
        ],
        primary_key: None,
        constraints: vec![],
        indexes: vec![],
        partition_by: Some(PartitionSpec::Range {
            columns: vec!["bucket".into()],
            collapse: true,
        }),
        runtime_options: None,
        schema: None,
        existence_guard: None,
    }
}

fn create_range_partition(name: &str, from: PartitionBoundValue, to: PartitionBoundValue) -> Op {
    Op::CreatePartition {
        attributes: zeroship_migrate_ir::attribute::CreatePartitionAttributes::new(),
        name: name.into(),
        of: "events".into(),
        bounds: PartitionBounds::Range {
            from: vec![from],
            to: vec![to],
        },
        schema: None,
        existence_guard: None,
    }
}

fn attach_range_partition(name: &str, from: PartitionBoundValue, to: PartitionBoundValue) -> Op {
    Op::AttachPartition {
        parent: "events".into(),
        name: name.into(),
        bound: PartitionBounds::Range {
            from: vec![from],
            to: vec![to],
        },
        schema: None,
    }
}

fn create_default_partition() -> Op {
    Op::CreatePartition {
        attributes: zeroship_migrate_ir::attribute::CreatePartitionAttributes::new(),
        name: "events_default".into(),
        of: "events".into(),
        bounds: PartitionBounds::Default,
        schema: None,
        existence_guard: None,
    }
}

fn drop_events_partition(name: &str) -> Op {
    Op::DropPartition {
        parent: "events".into(),
        name: name.into(),
        schema: None,
        existence_guard: None,
        cascade: None,
    }
}

fn insert_events(rows: &[(i64, &str)]) -> Op {
    Op::Insert {
        table: "events".into(),
        columns: vec!["bucket".into(), "payload".into()],
        rows: rows
            .iter()
            .map(|(bucket, payload)| {
                vec![
                    IrValue::from(IrScalar::Int(*bucket)),
                    IrValue::from(IrScalar::Str((*payload).into())),
                ]
            })
            .collect(),
        on_conflict: None,
        schema: None,
    }
}

fn partition_live_from_fold(ops: &[Op]) -> LiveSchema {
    let snap = fold_ops(
        zeroship_migrate::shipping_vendors(),
        ops,
        &zeroship_migrate_sqlite::DIALECT,
        "prj_partition",
        &support::no_inject("app"),
    )
    .expect("fold partition ops");
    let mut live = LiveSchema::from_tables(snap.tables.keys().cloned().collect());
    live.table_snapshots = snap.tables;
    live.partitions = snap.partitions;
    live
}

fn partition_exec_cfg() -> ExecutorConfig {
    ExecutorConfig::new(
        "prj_partition",
        "app_partition",
        support::no_inject("app_partition"),
    )
}

#[track_caller]
fn lower_sqlite_partition_steps(ops: Vec<Op>, live: &LiveSchema) -> Vec<zeroship_migrate::PlanStep> {
    // Applied calls in this fixture represent separate migration files. Give
    // each call site its own durable name so stable plan identities do not turn
    // unrelated setup and teardown plans into checksum drift.
    let mut migration = ir_ops(ops);
    migration.name = format!("partition_render_{}", std::panic::Location::caller().line());
    IrAuthor::new(
        zeroship_migrate::shipping_vendors(),
        "prj_partition",
        "app_partition",
        &zeroship_migrate_sqlite::DIALECT,
        &support::no_inject("app"),
    )
    .lower_steps(&migration, live)
    .expect("lower partition ops to SQLite")
}

fn rendered_partition_sql(steps: &[zeroship_migrate::PlanStep]) -> String {
    steps
        .iter()
        .map(|step| match step {
            zeroship_migrate::PlanStep::Ddl(migration) => migration.up.clone(),
            zeroship_migrate::PlanStep::Dml { template, .. } => template.clone(),
            other => format!("{other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn apply_sqlite_partition_steps(
    backend: &SqliteBackend,
    steps: &[zeroship_migrate::PlanStep],
    approval: Approval,
) -> Result<zeroship_migrate::DeclarativeDeployOutcome, zeroship_migrate::DeclarativeApplyError> {
    let cfg = partition_exec_cfg();
    MigrationEngine::new(zeroship_migrate::shipping_vendors())
        .apply_plan(
            steps,
            approval,
            backend,
            &cfg,
            "partition-test",
            LockMode::Acquire,
        )
        .await
}

async fn sqlite_event_rows(backend: &SqliteBackend) -> Vec<(i64, String)> {
    backend
        .actor()
        .query("SELECT bucket, payload FROM events ORDER BY bucket")
        .await
        .expect("read events")
        .into_iter()
        .map(|row| {
            let bucket = row[0]
                .as_ref()
                .expect("bucket")
                .parse::<i64>()
                .expect("integer bucket");
            let payload = row[1].as_ref().expect("payload").clone();
            (bucket, payload)
        })
        .collect()
}

fn sqlite_partition_backend(label: &str) -> (tempfile::TempDir, SqliteBackend) {
    let dir = tempfile::tempdir().expect("sqlite tempdir");
    let app = dir.path().join(format!("{label}.sqlite"));
    let journal = dir.path().join(format!("{label}.migrations.sqlite"));
    let backend = SqliteBackend::open(&app, &journal).expect("open sqlite backend");
    (dir, backend)
}

fn collapse_events_ops() -> Vec<Op> {
    vec![
        create_events_parent(),
        create_range_partition("events_0", int_bound(0), int_bound(100)),
        create_default_partition(),
    ]
}

/// Index attributes in the wire form the op now carries, for fixtures that used to build
/// an `IndexStorageParams` literal.
fn index_attrs(pairs: &[(&str, i64)]) -> zeroship_migrate_ir::attribute::CreateIndexAttributes {
    let mut carried = zeroship_migrate_ir::attribute::Attributes::new();
    for (key, value) in pairs {
        carried.insert(
            zeroship_migrate_ir::attribute::AttrKey::parse(key).expect("a well-formed key"),
            zeroship_migrate::IrScalar::Int(*value),
        );
    }
    zeroship_migrate_ir::attribute::CreateIndexAttributes::from(carried)
}

fn create_index(
    using: Option<IndexMethod>,
    include: Vec<String>,
    attributes: zeroship_migrate_ir::attribute::CreateIndexAttributes,
    only: Option<bool>,
) -> Op {
    Op::CreateIndex {
        table: "events".into(),
        columns: vec![idx_col("created_at")],
        name: Some("events_created_at_idx".into()),
        unique: None,
        using,
        r#where: None,
        include,
        attributes,
        only,
        nulls_not_distinct: None,
        concurrently: None,
        schema: None,
        existence_guard: None,
    }
}

#[test]
fn render_partitioned_parent_create_table_pg() {
    let sql = pg_sql(Op::CreateTable {
        attributes: zeroship_migrate_ir::attribute::CreateTableAttributes::new(),
        name: "events".into(),
        columns: vec![col("created_at", ColType::Timestamp)],
        primary_key: None,
        constraints: vec![],
        indexes: vec![],
        partition_by: Some(PartitionSpec::Range {
            columns: vec!["created_at".into()],
            collapse: false,
        }),
        runtime_options: None,
        schema: None,
        existence_guard: None,
    })
    .join("\n");

    assert!(
        sql.contains("PARTITION BY RANGE (\"created_at\")"),
        "partition clause missing from SQL:\n{sql}"
    );
}

#[compio::test]
async fn collapse_affirmed_events_apply_as_plain_table_on_sqlite() {
    use zeroship_migrate::model::validate::validate_ir_scoped;
    use zeroship_migrate_sqlite::backend::Mode;

    let ops = collapse_events_ops();
    let migration_ir = ir_ops(ops);
    validate_ir_scoped(
        zeroship_migrate::shipping_vendors(),
        &migration_ir,
        &zeroship_migrate_sqlite::DIALECT,
        None,
    )
    .expect("collapse-affirmed partition recording validates on SQLite");

    let steps = IrAuthor::new(
        zeroship_migrate::shipping_vendors(),
        "prj_partition",
        "app_partition",
        &zeroship_migrate_sqlite::DIALECT,
        &support::no_inject("app"),
    )
    .lower_steps(&migration_ir, &LiveSchema::default())
    .expect("lower collapse-affirmed partition recording on SQLite");
    assert_eq!(
        steps.len(),
        2,
        "bounded createPartition lowers to a mirror guard DML step; default child remains no-DDL on SQLite"
    );
    let sql = rendered_partition_sql(&steps);
    assert!(
        sql.contains("partitionBy collapsed to a plain table"),
        "degraded leg should be visible in plan output:\n{sql}"
    );
    assert!(
        sql.contains("partition collapse populated-default mirror guard"),
        "bounded child create should carry the populated-default mirror guard:\n{sql}"
    );
    assert!(
        sql.contains("CREATE TABLE"),
        "parent table DDL missing:\n{sql}"
    );
    assert!(
        !sql.contains("PARTITION BY") && !sql.contains("PARTITION OF"),
        "SQLite collapse must not emit native partition syntax:\n{sql}"
    );
    assert!(
        !sql.contains("events_default"),
        "SQLite collapse must not emit child table DDL:\n{sql}"
    );

    let dir = tempfile::tempdir().expect("sqlite tempdir");
    let app = dir.path().join("collapse.sqlite");
    let journal = dir.path().join("collapse.migrations.sqlite");
    let backend = SqliteBackend::open(&app, &journal).expect("open sqlite backend");
    apply_sqlite_partition_steps(&backend, &steps, Approval::None)
        .await
        .expect("apply collapsed parent table on SQLite");
    backend
        .actor()
        .set_mode(Mode::CreatorUp)
        .await
        .expect("creator mode");
    backend
        .actor()
        .exec("INSERT INTO events (bucket, payload) VALUES (42, 'range'), (250, 'default')")
        .await
        .expect("insert rows into collapsed table");
    let rows = backend
        .actor()
        .query("SELECT bucket, payload FROM events ORDER BY bucket")
        .await
        .expect("read collapsed table rows");
    assert_eq!(
        rows,
        vec![
            vec![Some("42".to_string()), Some("range".to_string())],
            vec![Some("250".to_string()), Some("default".to_string())],
        ]
    );
    let children = backend
        .actor()
        .query(
            "SELECT name FROM sqlite_master WHERE type='table' AND name IN ('events_0','events_default')",
        )
        .await
        .expect("check child tables");
    assert!(
        children.is_empty(),
        "collapse child partitions must be no-DDL"
    );
}

#[compio::test]
async fn collapse_bounded_child_drop_deletes_bound_rows_on_sqlite() {
    let ops = vec![
        create_events_parent(),
        create_range_partition("events_0", int_bound(0), int_bound(100)),
        create_default_partition(),
        insert_events(&[(42, "range"), (150, "default-a"), (250, "default-b")]),
        drop_events_partition("events_0"),
    ];
    let steps = lower_sqlite_partition_steps(ops, &LiveSchema::default());
    let rendered = rendered_partition_sql(&steps);
    assert!(
        rendered.contains("partition child drop collapsed to DELETE FROM parent")
            && rendered
                .contains("DELETE FROM \"events\" WHERE \"bucket\" >= 0 AND \"bucket\" < 100"),
        "bounded child drop must render a bounded DELETE:\n{rendered}"
    );

    let (_dir, backend) = sqlite_partition_backend("bounded_drop");
    apply_sqlite_partition_steps(&backend, &steps, Approval::Approved)
        .await
        .expect("apply bounded child drop");
    assert_eq!(
        sqlite_event_rows(&backend).await,
        vec![
            (150, "default-a".to_string()),
            (250, "default-b".to_string()),
        ]
    );
}

#[compio::test]
async fn collapse_default_child_drop_deletes_residual_rows_on_sqlite() {
    let ops = vec![
        create_events_parent(),
        create_range_partition("events_0", int_bound(0), int_bound(100)),
        create_default_partition(),
        insert_events(&[(42, "range"), (150, "default-a"), (250, "default-b")]),
        drop_events_partition("events_default"),
    ];
    let steps = lower_sqlite_partition_steps(ops, &LiveSchema::default());
    let rendered = rendered_partition_sql(&steps);
    assert!(
        rendered
            .contains("DELETE FROM \"events\" WHERE NOT (\"bucket\" >= 0 AND \"bucket\" < 100)"),
        "default child drop must render residual sibling negation:\n{rendered}"
    );

    let (_dir, backend) = sqlite_partition_backend("default_drop");
    apply_sqlite_partition_steps(&backend, &steps, Approval::Approved)
        .await
        .expect("apply default child drop realization");
    assert_eq!(
        sqlite_event_rows(&backend).await,
        vec![(42, "range".to_string())]
    );
}

#[compio::test]
async fn collapse_create_bounded_child_mirror_guard_errors_only_when_default_has_matching_rows_sqlite(
) {
    let dirty_ops = vec![
        create_events_parent(),
        create_default_partition(),
        insert_events(&[(42, "stray")]),
        create_range_partition("events_0", int_bound(0), int_bound(100)),
    ];
    let dirty_steps = lower_sqlite_partition_steps(dirty_ops, &LiveSchema::default());
    let dirty_rendered = rendered_partition_sql(&dirty_steps);
    assert!(
        dirty_rendered.contains("partition collapse populated-default mirror guard")
            && dirty_rendered.contains("INSERT INTO \"events\" (\"bucket\") SELECT NULL"),
        "bounded create must carry the populated-default mirror guard:\n{dirty_rendered}"
    );
    let (_dir, dirty_backend) = sqlite_partition_backend("mirror_dirty");
    let err = apply_sqlite_partition_steps(&dirty_backend, &dirty_steps, Approval::None)
        .await
        .expect_err("matching default rows must trip the mirror guard");
    assert!(
        format!("{err:?}").contains("NOT NULL") || format!("{err:?}").contains("constraint"),
        "mirror guard should fail closed through the NOT NULL key insert, got {err:?}"
    );

    let clean_ops = vec![
        create_events_parent(),
        create_default_partition(),
        insert_events(&[(250, "default")]),
        create_range_partition("events_0", int_bound(0), int_bound(100)),
    ];
    let clean_steps = lower_sqlite_partition_steps(clean_ops, &LiveSchema::default());
    let (_dir, clean_backend) = sqlite_partition_backend("mirror_clean");
    apply_sqlite_partition_steps(&clean_backend, &clean_steps, Approval::None)
        .await
        .expect("non-matching default rows must not trip the mirror guard");
    assert_eq!(
        sqlite_event_rows(&clean_backend).await,
        vec![(250, "default".to_string())]
    );
}

#[compio::test]
async fn collapse_child_create_down_drops_rows_before_parent_drop_on_sqlite() {
    let up_ops = vec![
        create_events_parent(),
        create_range_partition("events_0", int_bound(0), int_bound(100)),
        create_default_partition(),
        insert_events(&[(42, "range"), (150, "default-a"), (250, "default-b")]),
    ];
    let up_steps = lower_sqlite_partition_steps(up_ops.clone(), &LiveSchema::default());
    let (_dir, backend) = sqlite_partition_backend("auto_down");
    apply_sqlite_partition_steps(&backend, &up_steps, Approval::None)
        .await
        .expect("apply partitioned up migration");
    assert_eq!(
        sqlite_event_rows(&backend).await,
        vec![
            (42, "range".to_string()),
            (150, "default-a".to_string()),
            (250, "default-b".to_string()),
        ]
    );

    let live = partition_live_from_fold(&up_ops);
    let down_child_ops = vec![
        drop_events_partition("events_default"),
        drop_events_partition("events_0"),
    ];
    let down_child_steps = lower_sqlite_partition_steps(down_child_ops, &live);
    let rendered = rendered_partition_sql(&down_child_steps);
    assert!(
        rendered
            .contains("DELETE FROM \"events\" WHERE NOT (\"bucket\" >= 0 AND \"bucket\" < 100)")
            && rendered
                .contains("DELETE FROM \"events\" WHERE \"bucket\" >= 0 AND \"bucket\" < 100"),
        "child-create down must realize semantic child drops:\n{rendered}"
    );
    apply_sqlite_partition_steps(&backend, &down_child_steps, Approval::Approved)
        .await
        .expect("apply semantic child drops");
    assert!(
        sqlite_event_rows(&backend).await.is_empty(),
        "child drops remove all rows"
    );

    let drop_parent_steps = lower_sqlite_partition_steps(
        vec![Op::DropTable {
            table: "events".into(),
            cascade: None,
            schema: None,
            existence_guard: None,
        }],
        &LiveSchema::default(),
    );
    apply_sqlite_partition_steps(&backend, &drop_parent_steps, Approval::Approved)
        .await
        .expect("drop collapsed parent table");
    let tables = backend
        .actor()
        .query("SELECT name FROM sqlite_master WHERE type='table' AND name='events'")
        .await
        .expect("check events table");
    assert!(tables.is_empty(), "events table should be gone after down");
}

#[compio::test]
async fn collapse_range_min_value_omits_lower_delete_bound_on_sqlite() {
    let ops = vec![
        create_events_parent(),
        create_range_partition(
            "events_lt_100",
            PartitionBoundValue::MinValue,
            int_bound(100),
        ),
        create_range_partition(
            "events_gte_100",
            int_bound(100),
            PartitionBoundValue::MaxValue,
        ),
        create_default_partition(),
        insert_events(&[(-10, "min-a"), (42, "min-b"), (150, "max")]),
        drop_events_partition("events_lt_100"),
    ];
    let steps = lower_sqlite_partition_steps(ops, &LiveSchema::default());
    let rendered = rendered_partition_sql(&steps);
    assert!(
        rendered.contains("DELETE FROM \"events\" WHERE \"bucket\" < 100")
            && !rendered.contains("DELETE FROM \"events\" WHERE \"bucket\" >= MINVALUE"),
        "minValue range delete should omit the lower bound:\n{rendered}"
    );

    let (_dir, backend) = sqlite_partition_backend("min_value");
    apply_sqlite_partition_steps(&backend, &steps, Approval::Approved)
        .await
        .expect("apply minValue delete");
    assert_eq!(
        sqlite_event_rows(&backend).await,
        vec![(150, "max".to_string())]
    );
}

#[test]
fn render_create_partition_range_pg_dump_timestamptz_bounds() {
    let sql = pg_sql(Op::CreatePartition {
        attributes: zeroship_migrate_ir::attribute::CreatePartitionAttributes::new(),
        name: "events_2026_05".into(),
        of: "events".into(),
        bounds: PartitionBounds::Range {
            from: vec![PartitionBoundValue::String {
                value: "2026-05-01T00:00:00Z".into(),
            }],
            to: vec![PartitionBoundValue::String {
                value: "2026-06-01 00:00:00+00:00".into(),
            }],
        },
        schema: None,
        existence_guard: None,
    })
    .join("\n");

    assert!(
        sql.contains(
            "CREATE TABLE \"app\".\"events_2026_05\" PARTITION OF \"app\".\"events\" FOR VALUES FROM ('2026-05-01 00:00:00+00') TO ('2026-06-01 00:00:00+00')"
        ),
        "range partition SQL was:\n{sql}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// PG-vendor index enrichments: NULLS NOT DISTINCT + per-element opclass/collation
// (additive, PG-only; fail-closed off PostgreSQL).
// ─────────────────────────────────────────────────────────────────────────────

/// A createIndex over a single column carrying the given per-element facets +
/// index-level flags. `name` fixes the index name so the SQL is deterministic.
fn create_index_enriched(
    name: &str,
    element: IndexElement,
    unique: Option<bool>,
    nulls_not_distinct: Option<bool>,
) -> Op {
    Op::CreateIndex {
        table: "events".into(),
        columns: vec![element],
        name: Some(name.into()),
        unique,
        using: None,
        r#where: None,
        include: Vec::new(),
        attributes: Default::default(),
        only: None,
        nulls_not_distinct,
        concurrently: None,
        schema: None,
        existence_guard: None,
    }
}

fn col_opclass(name: &str, opclass: Option<&str>, collation: Option<&str>) -> IndexElement {
    IndexElement::Column {
        name: name.into(),
        order: None,
        opclass: opclass.map(str::to_string),
        collation: collation.map(str::to_string),
    }
}

#[test]
fn render_unique_index_nulls_not_distinct_pg() {
    let sql = pg_sql(create_index_enriched(
        "events_email_uq",
        col_opclass("email", None, None),
        Some(true),
        Some(true),
    ))
    .join("\n");
    assert!(
        sql.contains(
            "CREATE UNIQUE INDEX IF NOT EXISTS \"events_email_uq\" ON \"app\".\"events\" (\"email\") NULLS NOT DISTINCT"
        ),
        "NULLS NOT DISTINCT unique index SQL was:\n{sql}"
    );
}

#[test]
fn render_index_element_opclass_pg() {
    let sql = pg_sql(create_index_enriched(
        "events_email_pat",
        col_opclass("email", Some("text_pattern_ops"), None),
        None,
        None,
    ))
    .join("\n");
    assert!(
        sql.contains("(\"email\" text_pattern_ops)"),
        "per-element opclass SQL was:\n{sql}"
    );
}

#[test]
fn render_index_element_collation_pg() {
    let sql = pg_sql(create_index_enriched(
        "events_email_coll",
        col_opclass("email", None, Some("C")),
        None,
        None,
    ))
    .join("\n");
    assert!(
        sql.contains("(\"email\" COLLATE \"C\")"),
        "per-element collation SQL was:\n{sql}"
    );
}

#[test]
fn render_index_element_collation_precedes_opclass_pg() {
    // PG index-element grammar: `<col> COLLATE "<c>" <opclass>`.
    let sql = pg_sql(create_index_enriched(
        "events_email_both",
        col_opclass("email", Some("text_pattern_ops"), Some("C")),
        None,
        None,
    ))
    .join("\n");
    assert!(
        sql.contains("(\"email\" COLLATE \"C\" text_pattern_ops)"),
        "COLLATE must precede opclass; SQL was:\n{sql}"
    );
}

#[test]
fn pg_vendor_index_features_refused_fail_closed_off_pg() {
    use zeroship_migrate::model::validate::{validate_ir, CODE_UNSUPPORTED};

    let cases: Vec<(&str, Op)> = vec![
        (
            "nullsNotDistinct",
            create_index_enriched(
                "events_email_uq",
                col_opclass("email", None, None),
                Some(true),
                Some(true),
            ),
        ),
        (
            "opclass",
            create_index_enriched(
                "events_email_pat",
                col_opclass("email", Some("text_pattern_ops"), None),
                None,
                None,
            ),
        ),
        (
            "collation",
            create_index_enriched(
                "events_email_coll",
                col_opclass("email", None, Some("C")),
                None,
                None,
            ),
        ),
    ];

    for (label, op) in cases {
        let migration = ir(op);
        for dialect in [&zeroship_migrate_sqlite::DIALECT, &zeroship_migrate_mysql::DIALECT] {
            let err = validate_ir(zeroship_migrate::shipping_vendors(), &migration, dialect)
                .expect_err(&format!("{label} must be refused on {dialect:?}"));
            assert_eq!(
                err.code, CODE_UNSUPPORTED,
                "{label} on {dialect:?} must fail closed as UNSUPPORTED, got {err:?}"
            );
        }
        // The same op validates cleanly on PostgreSQL.
        validate_ir(
            zeroship_migrate::shipping_vendors(),
            &migration,
            &zeroship_migrate_postgres::DIALECT,
        )
        .unwrap_or_else(|e| panic!("{label} must validate on Postgres: {e:?}"));
    }
}

#[test]
fn render_create_partition_default_pg() {
    let sql = pg_sql(Op::CreatePartition {
        attributes: zeroship_migrate_ir::attribute::CreatePartitionAttributes::new(),
        name: "events_default".into(),
        of: "events".into(),
        bounds: PartitionBounds::Default,
        schema: None,
        existence_guard: None,
    })
    .join("\n");

    assert!(
        sql.contains(
            "CREATE TABLE \"app\".\"events_default\" PARTITION OF \"app\".\"events\" DEFAULT"
        ),
        "default partition SQL was:\n{sql}"
    );
}

#[test]
fn render_detach_partition_pg() {
    let sql = pg_sql(Op::DetachPartition {
        parent: "events".into(),
        name: "events_default".into(),
        schema: None,
        concurrently: Some(true),
    })
    .join("\n");

    assert!(
        sql.contains("ALTER TABLE \"app\".\"events\" DETACH PARTITION \"app\".\"events_default\" CONCURRENTLY"),
        "detach SQL was:\n{sql}"
    );
}

/// A detach is gated exactly as its sibling drop is, because one classifier already
/// says they are the same kind of thing.
///
/// `Op::is_destructive` lists `DetachPartition` and `DropPartition` together, under one
/// comment reading "partition drop / detach". The posture walk in
/// `check_ir_data_security_policy` acts on that: under
/// `data_security.destructive_ops = forbid` BOTH are refused, because `posture_denies`
/// excludes only `Update`/`Delete`/`Backfill`/`Raw`.
///
/// The flags the author attaches are the OTHER gate, and they are what the approval
/// path reads — `PlanStep::approval_scope_version` fires on
/// `flags.destructive || flags.requires_approval`. So a detach that carries neither is
/// ungated on that path while every sibling drop is gated, and a detach lowers with
/// `down = None`, which means the engine cannot reverse what it did.
///
/// This is asserted DIFFERENTIALLY rather than against the literal flag values. The
/// claim worth holding is that the two agree; if the project later changes what a
/// destructive drop's flags are, this must follow that change rather than pin a stale
/// pair.
#[test]
fn a_detach_partition_is_gated_like_the_drop_it_is_classified_with() {
    let detach = pg_lower(Op::DetachPartition {
        parent: "events".into(),
        name: "events_default".into(),
        schema: None,
        concurrently: Some(true),
    });
    let drop = pg_lower(Op::DropPartition {
        parent: "events".into(),
        name: "events_default".into(),
        schema: None,
        existence_guard: None,
        cascade: None,
    });

    let detach_flags = detach
        .first()
        .expect("a detachPartition lowers to at least one migration")
        .flags;
    let drop_flags = drop
        .first()
        .expect("a dropPartition lowers to at least one migration")
        .flags;

    assert!(
        drop_flags.destructive && drop_flags.requires_approval,
        "the control failed: `dropPartition` is supposed to be the GATED sibling, so if \
         it is not destructive-and-approval-gated then this test is comparing against \
         nothing. Got {drop_flags:?}"
    );

    assert_eq!(
        detach_flags.destructive, drop_flags.destructive,
        "`detachPartition` and `dropPartition` are one family to `Op::is_destructive`, \
         and the posture walk refuses BOTH under `destructive_ops = forbid`. The \
         approval path reads these flags instead, so a detach that is not marked \
         destructive is ungated there while its sibling is gated — and a detach lowers \
         with no `down`, so nothing can undo it either."
    );
    assert_eq!(
        detach_flags.requires_approval, drop_flags.requires_approval,
        "same family, same approval requirement: `PlanStep::approval_scope_version` \
         fires on `destructive || requires_approval`, so this is the field that decides \
         whether a human sees the operation at all"
    );
}

#[test]
fn render_attach_partition_pg() {
    let sql = pg_sql(attach_range_partition(
        "events_100_200",
        int_bound(100),
        int_bound(200),
    ))
    .join("\n");

    assert!(
        sql.contains(
            "ALTER TABLE \"app\".\"events\" ATTACH PARTITION \"app\".\"events_100_200\" FOR VALUES FROM (100) TO (200)"
        ),
        "attach SQL was:\n{sql}"
    );
}

#[test]
fn attach_partition_refused_fail_closed_off_pg() {
    use zeroship_migrate::model::validate::{
        validate_ir_scoped, CODE_UNSUPPORTED, CODE_VENDOR_OP_DENIED,
    };

    let migration = ir(attach_range_partition(
        "events_100_200",
        int_bound(100),
        int_bound(200),
    ));
    for dialect in [&zeroship_migrate_sqlite::DIALECT, &zeroship_migrate_mysql::DIALECT] {
        let err = validate_ir_scoped(zeroship_migrate::shipping_vendors(), &migration, dialect, None)
            .expect_err(&format!("attachPartition must be refused on {dialect:?}"));
        assert!(
            matches!(err.code.as_str(), CODE_UNSUPPORTED | CODE_VENDOR_OP_DENIED),
            "attachPartition on {dialect:?} must fail closed as PG-only/vendor, got {err:?}"
        );
    }
}

#[test]
fn render_drop_partition_pg() {
    let sql = pg_sql(Op::DropPartition {
        parent: "events".into(),
        name: "events_default".into(),
        schema: None,
        existence_guard: None,
        cascade: Some(true),
    })
    .join("\n");

    assert!(
        sql.contains("DROP TABLE \"app\".\"events_default\" CASCADE"),
        "drop partition SQL was:\n{sql}"
    );
}

#[test]
fn render_brin_index_pg() {
    let sql = pg_sql(create_index(
        Some(IndexMethod::Brin),
        vec![],
        index_attrs(&[]),
        None,
    ))
    .join("\n");
    assert!(sql.contains("USING brin"), "BRIN index SQL was:\n{sql}");
}

#[test]
fn render_include_index_pg() {
    let sql = pg_sql(create_index(
        None,
        vec!["kind".into()],
        index_attrs(&[]),
        None,
    ))
    .join("\n");
    assert!(
        sql.contains("INCLUDE (\"kind\")"),
        "INCLUDE index SQL was:\n{sql}"
    );
}

#[test]
fn render_with_storage_param_index_pg() {
    let sql = pg_sql(create_index(
        Some(IndexMethod::Brin),
        vec![],
        index_attrs(&[
            ("postgres.pages_per_range", 32),
            ("postgres.fillfactor", 70),
        ]),
        None,
    ))
    .join("\n");

    assert!(
        // CANONICAL KEY ORDER, alphabetical by full key, so `fillfactor` precedes
        // `pages_per_range`. This used to be the order two struct fields were declared in.
        // Both spellings are the same statement to PostgreSQL; a canonical one is what
        // keeps rendered DDL reproducible now that the set of parameters is open.
        sql.contains("WITH (fillfactor='70', pages_per_range='32')"),
        "WITH storage param SQL was:\n{sql}"
    );
}

#[test]
fn render_on_only_index_pg() {
    let sql = pg_sql(create_index(None, vec![], index_attrs(&[]), Some(true))).join("\n");
    assert!(
        sql.contains("ON ONLY \"app\".\"events\""),
        "ON ONLY SQL was:\n{sql}"
    );
}
