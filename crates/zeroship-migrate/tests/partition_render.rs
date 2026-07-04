use zeroship_migrate::model::ir::{
    ColType, IndexElement, IndexMethod, IndexStorageParams, IrColumn, MigrationIr, Op,
    PartitionBoundValue, PartitionBounds, PartitionSpec,
};
use zeroship_migrate::{
    IrAuthor, IrFlagsOverride, LiveSchema, SqlDialect, CURRENT_IR_VERSION,
};

fn col(name: &str, ty: ColType) -> IrColumn {
    IrColumn {
        name: name.into(),
        ty,
        nullable: None,
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
    }
}

fn ir(op: Op) -> MigrationIr {
    MigrationIr {
        ir_version: CURRENT_IR_VERSION,
        name: "partition_render".into(),
        owner_app: "app_partition".into(),
        ops: vec![op],
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
