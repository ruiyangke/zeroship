//! ADVERSARIAL probe (review of mig-first P1 `fold_ops`): construct op streams the
//! headline oracle's corpus does NOT exercise and check `fold_ops == introspect`.
//!
//! Focus: PG auto-drops a column's dependent indexes / constraints when the column
//! is dropped (`ALTER TABLE … DROP COLUMN` cascades to indexes + UNIQUE/FK that
//! reference it). Does the fold's `DropColumn` arm mirror that? It only `retain`s
//! columns today, so this is the prime divergence candidate.
//!
//! Reuses the same real pipeline harness as `fold_roundtrip_pg.rs`.

use compio_postgres::Client;
use zeroship_migrate::{
    drift::snapshot_schema, executor::LockMode, fold_ops, ir::Op, provision_migrator,
    role::deprovision_migrator, Approval, ExecutorConfig, IrAuthor, LiveSchema, MigrationEngine,
    PostgresBackend, SchemaSnapshot, SqlDialect,
};

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";

fn dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| DEFAULT_DSN.to_string())
}
fn require_db() -> bool {
    std::env::var("MIGRATE_REQUIRE_DB").is_ok()
}
async fn pg() -> Client {
    let (client, conn) = compio_postgres::connect(&dsn(), compio_postgres::NoTls)
        .await
        .expect("connect");
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
const APP: &str = "app_test";
fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.pg.meta_schema = format!("meta_{tok}");
    let role = zeroship_migrate::migrator_role_name(&c.project_id).unwrap();
    c.with_migrator_role(role)
}
async fn setup(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{}\"", cfg.project_schema))
        .await
        .unwrap();
    provision_migrator(conn, cfg).await.unwrap();
}
async fn teardown(conn: &Client, cfg: &ExecutorConfig) {
    let _ = deprovision_migrator(conn, cfg).await;
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}
fn registry(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
    pairs.iter().map(|(t, o)| (t.to_string(), o.to_string())).collect()
}
fn live_from_snapshot(snap: &SchemaSnapshot, owner: &str) -> LiveSchema {
    let mut live = LiveSchema::from_tables(snap.tables.keys().cloned().collect());
    live.unique_indexes = snap
        .tables
        .values()
        .flat_map(|t| t.indexes.iter())
        .filter(|i| i.unique)
        .map(|i| i.name.clone())
        .collect();
    live.table_snapshots = snap.tables.clone();
    live.table_ownership = snap.tables.keys().map(|t| (t.clone(), owner.to_string())).collect();
    live
}
async fn apply_doc(
    conn: &Client,
    cfg: &ExecutorConfig,
    ir: &str,
    reg: &std::collections::BTreeMap<String, String>,
    approval: Approval,
) -> Vec<Op> {
    let live_snap = snapshot_schema(conn, &cfg.project_schema).await.unwrap();
    let live = live_from_snapshot(&live_snap, APP);
    let author = IrAuthor::new(cfg.project_schema.clone(), APP, SqlDialect::Postgres);
    let document = zeroship_migrate::ir_load::load_ir_document(
        ir,
        APP,
        zeroship_migrate::validate::Dialect::Postgres,
        reg,
        None,
    )
    .expect("load gate");
    let ops = document.ops.clone();
    let plan = author.lower_plan(&document, &live).expect("lower");
    let engine = MigrationEngine::new();
    engine
        .apply_plan(&plan.steps, approval, &PostgresBackend::new(conn), cfg, APP, LockMode::Acquire)
        .await
        .expect("apply");
    ops
}
fn canonicalize(mut snap: SchemaSnapshot) -> SchemaSnapshot {
    for t in snap.tables.values_mut() {
        for c in &mut t.constraints {
            if c.kind == "CHECK" {
                c.definition = String::new();
            }
        }
    }
    snap
}

/// ADVERSARIAL #A — create an index on a column, then DROP that column. PG
/// auto-drops the dependent index; does the fold?
///
/// REVIEW FINDING (HIGH) — FIXED: the fold's `DropColumn` arm now cascades to
/// dependent indexes (and UNIQUE/FK constraints + their implicit indexes), mirroring
/// PG's `ALTER TABLE … DROP COLUMN` auto-cascade. This test PROVES `fold_ops ==
/// introspect` after a column with a dependent index is dropped (it FAILED before
/// the fix: a phantom `t_b_idx` survived in the fold).
#[compio::test]
async fn drop_column_cascades_dependent_index() {
    if !require_db() {
        eprintln!("SKIP drop_column_cascades_dependent_index (MIGRATE_REQUIRE_DB unset)");
        return;
    }
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let mut all: Vec<Op> = Vec::new();

    let mk = r#"{"ir_version":1,"name":"mk","ops":[
        {"op":"createTable","name":"t","columns":[
            {"name":"a","type":"text","nullable":false},
            {"name":"b","type":"text","nullable":true}
        ]}
    ]}"#;
    all.extend(apply_doc(&conn, &cfg, mk, &registry(&[]), Approval::None).await);

    let reg = registry(&[("t", APP)]);
    let idx = r#"{"ir_version":1,"name":"mkidx","ops":[
        {"op":"createIndex","table":"t","columns":["b"],"name":"t_b_idx"}
    ]}"#;
    all.extend(apply_doc(&conn, &cfg, idx, &reg, Approval::None).await);

    let drop_b = r#"{"ir_version":1,"name":"dropb","ops":[
        {"op":"dropColumn","table":"t","column":"b"}
    ]}"#;
    all.extend(apply_doc(&conn, &cfg, drop_b, &reg, Approval::Approved).await);

    let live = snapshot_schema(&conn, &cfg.project_schema).await.unwrap();
    let folded = fold_ops(&all, SqlDialect::Postgres, &cfg.project_schema).expect("fold");

    assert_eq!(
        canonicalize(folded),
        canonicalize(live),
        "fold must drop the index PG auto-dropped with the column"
    );
    teardown(&conn, &cfg).await;
}

/// ADVERSARIAL #B — add a UNIQUE constraint on a column, then DROP that column. PG
/// auto-drops the dependent UNIQUE constraint + its implicit index. Does the fold?
///
/// REVIEW FINDING (HIGH) — FIXED (same root cause as #A): the fold now drops both
/// the `t_b_uq` UNIQUE constraint AND its implicit index when the underlying column
/// `b` is dropped, matching live introspection (which shows neither).
#[compio::test]
async fn drop_column_cascades_dependent_unique_constraint() {
    if !require_db() {
        eprintln!("SKIP drop_column_cascades_dependent_unique_constraint (MIGRATE_REQUIRE_DB unset)");
        return;
    }
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let mut all: Vec<Op> = Vec::new();

    let mk = r#"{"ir_version":1,"name":"mk","ops":[
        {"op":"createTable","name":"t","columns":[
            {"name":"a","type":"text","nullable":false},
            {"name":"b","type":"text","nullable":false}
        ]}
    ]}"#;
    all.extend(apply_doc(&conn, &cfg, mk, &registry(&[]), Approval::None).await);

    let reg = registry(&[("t", APP)]);
    let uq = r#"{"ir_version":1,"name":"adduq","ops":[
        {"op":"addConstraint","table":"t",
            "constraint":{"name":"t_b_uq","kind":{"kind":"unique","columns":["b"]}}}
    ]}"#;
    all.extend(apply_doc(&conn, &cfg, uq, &reg, Approval::Approved).await);

    let drop_b = r#"{"ir_version":1,"name":"dropb","ops":[
        {"op":"dropColumn","table":"t","column":"b"}
    ]}"#;
    all.extend(apply_doc(&conn, &cfg, drop_b, &reg, Approval::Approved).await);

    let live = snapshot_schema(&conn, &cfg.project_schema).await.unwrap();
    let folded = fold_ops(&all, SqlDialect::Postgres, &cfg.project_schema).expect("fold");

    assert_eq!(
        canonicalize(folded),
        canonicalize(live),
        "fold must drop the UNIQUE constraint + its index PG auto-dropped with the column"
    );
    teardown(&conn, &cfg).await;
}

/// ADVERSARIAL #C — interleave: addColumn, then alterColumnType + alterColumnNullability
/// on the SAME freshly-added column, chained across docs. The headline corpus alters
/// only original createTable columns; this chains alters onto an added column.
#[compio::test]
async fn add_then_alter_same_column_chained() {
    if !require_db() {
        eprintln!("SKIP add_then_alter_same_column_chained (MIGRATE_REQUIRE_DB unset)");
        return;
    }
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let mut all: Vec<Op> = Vec::new();

    let mk = r#"{"ir_version":1,"name":"mk","ops":[
        {"op":"createTable","name":"t","columns":[
            {"name":"a","type":"text","nullable":false}
        ]}
    ]}"#;
    all.extend(apply_doc(&conn, &cfg, mk, &registry(&[]), Approval::None).await);
    let reg = registry(&[("t", APP)]);

    let add = r#"{"ir_version":1,"name":"add","ops":[
        {"op":"addColumn","table":"t","column":"n","type":"int","nullable":true}
    ]}"#;
    all.extend(apply_doc(&conn, &cfg, add, &reg, Approval::None).await);

    let alter = r#"{"ir_version":1,"name":"alter","ops":[
        {"op":"alterColumnType","table":"t","column":"n","type":"bigInt"},
        {"op":"alterColumnNullability","table":"t","column":"n","nullable":false}
    ]}"#;
    all.extend(apply_doc(&conn, &cfg, alter, &reg, Approval::Approved).await);

    let live = snapshot_schema(&conn, &cfg.project_schema).await.unwrap();
    let folded = fold_ops(&all, SqlDialect::Postgres, &cfg.project_schema).expect("fold");
    assert_eq!(
        canonicalize(folded),
        canonicalize(live),
        "fold of add-then-chained-alter must match introspection"
    );
    teardown(&conn, &cfg).await;
}
