use zeroship_migrate::model::ir::{
    ColType, IndexElement, IndexMethod, IndexStorageParams, IrColumn, MigrationIr, Op,
    PartitionBoundValue, PartitionBounds, PartitionSpec, SafeI64,
};
use zeroship_migrate::{IrAuthor, IrFlagsOverride, LiveSchema, SqlDialect, SqliteBackend, CURRENT_IR_VERSION};

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
    IrAuthor::new("app", "app_partition", SqlDialect::Postgres)
        .lower(&ir(op), &LiveSchema::default())
        .expect("lower")
        .into_iter()
        .map(|m| m.up)
        .collect()
}

fn int_bound(value: i64) -> PartitionBoundValue {
    PartitionBoundValue::Int {
        value: SafeI64::new(value).expect("test partition bound is JS-safe"),
    }
}

fn collapse_events_ops() -> Vec<Op> {
    vec![
        Op::CreateTable {
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
        },
        Op::CreatePartition {
            name: "events_0".into(),
            of: "events".into(),
            bounds: PartitionBounds::Range {
                from: vec![int_bound(0)],
                to: vec![int_bound(100)],
            },
            schema: None,
            existence_guard: None,
        },
        Op::CreatePartition {
            name: "events_default".into(),
            of: "events".into(),
            bounds: PartitionBounds::Default,
            schema: None,
            existence_guard: None,
        },
    ]
}

fn create_index(
    using: Option<IndexMethod>,
    include: Vec<String>,
    with: Option<IndexStorageParams>,
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
        with,
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
    use zeroship_migrate::apply::backend::sqlite::Mode;
    use zeroship_migrate::model::validate::{validate_ir_scoped, Dialect};
    use zeroship_migrate::PolicyProfile;

    let ops = collapse_events_ops();
    let migration_ir = ir_ops(ops);
    validate_ir_scoped(
        &migration_ir,
        Dialect::Sqlite,
        &[],
        None,
        &PolicyProfile::platform(),
    )
    .expect("collapse-affirmed partition recording validates on SQLite");

    let migrations = IrAuthor::new("prj_demo", "app_partition", SqlDialect::Sqlite)
        .lower(&migration_ir, &LiveSchema::default())
        .expect("lower collapse-affirmed partition recording on SQLite");
    assert_eq!(
        migrations.len(),
        1,
        "child createPartition ops must lower to no DDL on SQLite"
    );
    let sql = &migrations[0].up;
    assert!(
        sql.contains("partitionBy collapsed to a plain table"),
        "degraded leg should be visible in plan output:\n{sql}"
    );
    assert!(sql.contains("CREATE TABLE"), "parent table DDL missing:\n{sql}");
    assert!(
        !sql.contains("PARTITION BY") && !sql.contains("PARTITION OF"),
        "SQLite collapse must not emit native partition syntax:\n{sql}"
    );
    assert!(
        !sql.contains("events_0") && !sql.contains("events_default"),
        "SQLite collapse must not emit child table DDL:\n{sql}"
    );

    let dir = tempfile::tempdir().expect("sqlite tempdir");
    let app = dir.path().join("collapse.sqlite");
    let journal = dir.path().join("collapse.migrations.sqlite");
    let backend = SqliteBackend::open(&app, &journal).expect("open sqlite backend");
    backend
        .apply_one_additive(&migrations[0], "tester")
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
    assert!(children.is_empty(), "collapse child partitions must be no-DDL");
}

#[test]
fn render_create_partition_range_pg_dump_timestamptz_bounds() {
    let sql = pg_sql(Op::CreatePartition {
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
        with: None,
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
    use zeroship_migrate::model::validate::{validate_ir, Dialect, CODE_UNSUPPORTED};

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
        for dialect in [Dialect::Sqlite, Dialect::Mysql] {
            let err = validate_ir(&migration, dialect, &[None])
                .expect_err(&format!("{label} must be refused on {dialect:?}"));
            assert_eq!(
                err.code, CODE_UNSUPPORTED,
                "{label} on {dialect:?} must fail closed as UNSUPPORTED, got {err:?}"
            );
        }
        // The same op validates cleanly on PostgreSQL.
        validate_ir(&migration, Dialect::Postgres, &[None])
            .unwrap_or_else(|e| panic!("{label} must validate on Postgres: {e:?}"));
    }
}

#[test]
fn render_create_partition_default_pg() {
    let sql = pg_sql(Op::CreatePartition {
        name: "events_default".into(),
        of: "events".into(),
        bounds: PartitionBounds::Default,
        schema: None,
        existence_guard: None,
    })
    .join("\n");

    assert!(
        sql.contains("CREATE TABLE \"app\".\"events_default\" PARTITION OF \"app\".\"events\" DEFAULT"),
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
    let sql = pg_sql(create_index(Some(IndexMethod::Brin), vec![], None, None)).join("\n");
    assert!(sql.contains("USING brin"), "BRIN index SQL was:\n{sql}");
}

#[test]
fn render_include_index_pg() {
    let sql = pg_sql(create_index(None, vec!["kind".into()], None, None)).join("\n");
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
        Some(IndexStorageParams {
            pages_per_range: Some(32),
            fillfactor: Some(70),
        }),
        None,
    ))
    .join("\n");

    assert!(
        sql.contains("WITH (pages_per_range='32', fillfactor='70')"),
        "WITH storage param SQL was:\n{sql}"
    );
}

#[test]
fn render_on_only_index_pg() {
    let sql = pg_sql(create_index(None, vec![], None, Some(true))).join("\n");
    assert!(sql.contains("ON ONLY \"app\".\"events\""), "ON ONLY SQL was:\n{sql}");
}
