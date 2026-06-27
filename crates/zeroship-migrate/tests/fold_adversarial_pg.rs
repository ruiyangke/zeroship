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
    apply::drift::snapshot_schema, apply::executor::LockMode, fold_ops, model::ir::Op, provision_migrator,
    apply::role::deprovision_migrator, Approval, ExecutorConfig, IrAuthor, LiveSchema, MigrationEngine,
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
    let document = zeroship_migrate::model::load::load_ir_document(
        ir,
        APP,
        zeroship_migrate::model::validate::Dialect::Postgres,
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
        {"op":"createIndex","table":"t","columns":[{"kind":"column","name":"b"}],"name":"t_b_idx"}
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

/// ADVERSARIAL #D — a FOREIGN KEY constraint and an INDEPENDENT user index SHARE a
/// name, then the FK constraint is dropped. PG allows the coexistence (a FK backs no
/// index, so the name is free for a real index) and `DROP CONSTRAINT <shared>` leaves
/// the index intact. The fold's `DropConstraint` arm must therefore NOT cascade-drop
/// the same-named index for a FK — only for UNIQUE / PRIMARY KEY (whose implicit
/// same-named index PG really does cascade).
///
/// REVIEW FINDING (MED) — FIXED: `DropConstraint` now captures the dropped
/// constraint's KIND and gates the implicit-index `retain` on UNIQUE / PRIMARY KEY.
/// Pre-fix the unconditional `retain(|i| &i.name != name)` phantom-dropped the user
/// index `shared_name`, making `fold_ops != snapshot_schema(live)`; this test FAILED
/// before the fix and passes after.
#[compio::test]
async fn drop_fk_constraint_keeps_same_named_user_index() {
    if !require_db() {
        eprintln!("SKIP drop_fk_constraint_keeps_same_named_user_index (MIGRATE_REQUIRE_DB unset)");
        return;
    }
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let mut all: Vec<Op> = Vec::new();

    // Parent table the FK references (its system `id`).
    let parent = r#"{"ir_version":1,"name":"mkparent","ops":[
        {"op":"createTable","name":"parent","columns":[
            {"name":"label","type":"text","nullable":false}
        ]}
    ]}"#;
    all.extend(apply_doc(&conn, &cfg, parent, &registry(&[]), Approval::None).await);

    // Child table with the FK local column.
    let child = r#"{"ir_version":1,"name":"mkchild","ops":[
        {"op":"createTable","name":"child","columns":[
            {"name":"parent_id","type":"text","nullable":false},
            {"name":"note","type":"text","nullable":true}
        ]}
    ]}"#;
    all.extend(apply_doc(&conn, &cfg, child, &registry(&[("parent", APP)]), Approval::None).await);

    let reg = registry(&[("parent", APP), ("child", APP)]);

    // Add a FOREIGN KEY constraint named `shared_name` (FK backs no index).
    let add_fk = r#"{"ir_version":1,"name":"addfk","ops":[
        {"op":"addConstraint","table":"child",
            "constraint":{"name":"shared_name","kind":{"kind":"fk","columns":["parent_id"],
                "referencesTable":"parent","referencesColumns":["id"]}}}
    ]}"#;
    all.extend(apply_doc(&conn, &cfg, add_fk, &reg, Approval::Approved).await);

    // Create an INDEPENDENT user index that SHARES the FK's name (PG allows this).
    let mk_idx = r#"{"ir_version":1,"name":"mkidx","ops":[
        {"op":"createIndex","table":"child","columns":[{"kind":"column","name":"note"}],"name":"shared_name"}
    ]}"#;
    all.extend(apply_doc(&conn, &cfg, mk_idx, &reg, Approval::None).await);

    // Drop the FK constraint. The same-named user index MUST survive.
    let drop_fk = r#"{"ir_version":1,"name":"dropfk","ops":[
        {"op":"dropConstraint","table":"child","name":"shared_name"}
    ]}"#;
    all.extend(apply_doc(&conn, &cfg, drop_fk, &reg, Approval::Approved).await);

    let live = snapshot_schema(&conn, &cfg.project_schema).await.unwrap();
    // Live-PG oracle: the index `shared_name` survives the FK drop.
    assert!(
        live.tables["child"].indexes.iter().any(|i| i.name == "shared_name"),
        "live PG must still carry the user index `shared_name` after dropping the same-named FK"
    );

    let folded = fold_ops(&all, SqlDialect::Postgres, &cfg.project_schema).expect("fold");
    assert!(
        folded.tables["child"].indexes.iter().any(|i| i.name == "shared_name"),
        "fold must NOT phantom-drop the same-named user index when a FK constraint is dropped"
    );
    assert_eq!(
        canonicalize(folded),
        canonicalize(live),
        "fold must match introspection: dropping a FK leaves an independent same-named index intact"
    );
    teardown(&conn, &cfg).await;
}

/// Live round-trip: a reserved-word LOCAL FK column (`order`) must fold to the
/// SAME `FOREIGN KEY ("order")` body `pg_get_constraintdef` reports. Pre-fix the
/// FK `definition` was built with the local column interpolated RAW, so the fold
/// emitted a bare `FOREIGN KEY (order)` — phantom-diffing the catalog (which
/// quotes the keyword) under `ConstraintSnapshot`'s FULL Eq, failing this oracle.
#[compio::test]
async fn reserved_word_local_fk_column_round_trips() {
    if !require_db() {
        eprintln!("SKIP reserved_word_local_fk_column_round_trips (MIGRATE_REQUIRE_DB unset)");
        return;
    }
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let mut all: Vec<Op> = Vec::new();

    // Parent table the FK references (its system `id`).
    let parent = r#"{"ir_version":1,"name":"mkparent","ops":[
        {"op":"createTable","name":"parent","columns":[
            {"name":"label","type":"text","nullable":false}
        ]}
    ]}"#;
    all.extend(apply_doc(&conn, &cfg, parent, &registry(&[]), Approval::None).await);

    // Child table whose FK LOCAL column is a reserved keyword (`order`).
    let child = r#"{"ir_version":1,"name":"mkchild","ops":[
        {"op":"createTable","name":"child","columns":[
            {"name":"order","type":"text","nullable":false}
        ]}
    ]}"#;
    all.extend(apply_doc(&conn, &cfg, child, &registry(&[("parent", APP)]), Approval::None).await);

    let reg = registry(&[("parent", APP), ("child", APP)]);

    // FK on the reserved-word local column → exercises `fk_definition_pg(field=order)`.
    let add_fk = r#"{"ir_version":1,"name":"addfk","ops":[
        {"op":"addConstraint","table":"child",
            "constraint":{"name":"child_order_fk","kind":{"kind":"fk","columns":["order"],
                "referencesTable":"parent","referencesColumns":["id"]}}}
    ]}"#;
    all.extend(apply_doc(&conn, &cfg, add_fk, &reg, Approval::Approved).await);

    let live = snapshot_schema(&conn, &cfg.project_schema).await.unwrap();
    let folded = fold_ops(&all, SqlDialect::Postgres, &cfg.project_schema).expect("fold");
    assert_eq!(
        canonicalize(folded),
        canonicalize(live),
        r#"fold must match introspection for a reserved-word local FK column (FOREIGN KEY ("order"))"#
    );
    teardown(&conn, &cfg).await;
}
