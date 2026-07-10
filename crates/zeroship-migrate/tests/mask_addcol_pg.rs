#![cfg(feature = "native-pg")]
//! **#173 + #174 — live-PG round-trip for carried column facets.**
//!
//! Two facet-completeness lifts, proven against REAL Postgres (not a stub):
//!
//! - **#174 standalone `.mask()`** — a plaintext column authored with
//!   `t.string().mask({ kind, classification })` now carries the mask on `IrColumn.mask`.
//!   The op lower emits the hidden `<col>_masked TEXT` sibling + the `__zsmask` COMMENT
//!   sentinel (the SAME shape `t.encrypted()`'s auto-mask uses, and the SAME the runtime
//!   `plugin-db introspect_schema.rs` reads). We apply such a `createTable` on PG, then
//!   assert (a) the LIVE DB grew the `_masked` sibling column, (b) the OFFLINE
//!   `fold_ops` reproduces that exact sibling (structural equality with the introspected
//!   live schema — proving the sibling flowed through the lower from the carried facet),
//!   and (c) `fold_to_field_defs` recovers the mask on the FieldDef.
//!
//! - **#173 AddColumn facets** — `Op::AddColumn` now carries `vectorMetric` + `mask`. We
//!   `addColumn` a masked column to a live table and assert the same three: the live
//!   `_masked` sibling, fold==introspect, and the recovered mask.
//!
//! Requires `:5440` + `MIGRATE_REQUIRE_DB=1`; run with `--test-threads=1`.

use compio_postgres::Client;
use zeroship_migrate::{
    apply::drift::snapshot_schema,
    apply::executor::LockMode,
    fold_ops, fold_to_field_defs,
    model::ir::Op,
    provision_migrator,
    apply::role::deprovision_migrator,
    Approval, ExecutorConfig, IrAuthor, LiveSchema, MigrationEngine, PostgresBackend, SchemaSnapshot,
    SqlDialect,
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
        .expect("create project schema");
    provision_migrator(conn, cfg).await.expect("provision migrator role");
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
    live.partitions = snap.partitions.clone();
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
    let live_snap = snapshot_schema(conn, &cfg.project_schema)
        .await
        .expect("introspect live schema before applying the doc");
    let live = live_from_snapshot(&live_snap, APP);

    let author = IrAuthor::new(cfg.project_schema.clone(), APP, SqlDialect::Postgres);
    let document = zeroship_migrate::model::load::load_ir_document(
        ir,
        APP,
        zeroship_migrate::model::validate::Dialect::Postgres,
        reg,
        None,
        None,
    )
    .expect("load gate");
    let ops = document.ops.clone();
    let plan = author.lower_plan(&document, &live).expect("lower the doc plan on PG");
    let engine = MigrationEngine::new();
    engine
        .apply_plan(
            &plan.steps,
            approval,
            &PostgresBackend::new(conn),
            cfg,
            APP,
            LockMode::Acquire,
        )
        .await
        .expect("apply the authored plan on PG");
    ops
}

/// Blank CHECK definitions before the structural compare (introspection noise — same
/// canonicalization the keystone oracle uses).
fn canonicalize(mut snap: SchemaSnapshot) -> SchemaSnapshot {
    snap.roles.clear();
    snap.schemas.clear();
    snap.extensions.clear();
    for t in snap.tables.values_mut() {
        for c in &mut t.constraints {
            if c.kind == "CHECK" {
                c.definition = String::new();
            }
        }
    }
    snap
}

/// True iff `table` has a physical `<col>_masked` sibling column.
fn has_masked_sibling(snap: &SchemaSnapshot, table: &str, col: &str) -> bool {
    snap.tables
        .get(table)
        .map(|t| t.columns.iter().any(|c| c.name == format!("{col}_masked")))
        .unwrap_or(false)
}

/// **#174 + #173** — apply a standalone-masked `createTable` AND a masked `addColumn` on
/// real PG; assert the live `_masked` siblings exist, the offline fold reproduces the
/// SAME schema, and `fold_to_field_defs` recovers both masks.
#[compio::test]
async fn standalone_and_added_mask_round_trip_pg() {
    if !require_db() {
        eprintln!("SKIP standalone_and_added_mask_round_trip_pg (MIGRATE_REQUIRE_DB unset)");
        return;
    }
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let mut all_ops: Vec<Op> = Vec::new();

    // (1) createTable with a STANDALONE mask on a PLAINTEXT column (#174). `ssn` is a
    //     plain text column carrying `mask:{ last4, spi }` — NOT encrypted, so the only
    //     source of the mask is the carried `IrColumn.mask` facet.
    let people = r#"{"ir_version":1,"name":"create_people","ops":[
        {"op":"createTable","name":"people","columns":[
            {"name":"name","type":"text","nullable":false},
            {"name":"ssn","type":"text","nullable":true,"mask":{"kind":"last4","classification":"spi"}}
        ]}
    ]}"#;
    all_ops.extend(apply_doc(&conn, &cfg, people, &registry(&[]), Approval::None).await);

    // (2) addColumn a MASKED column to the live table (#173 — Op::AddColumn carries mask).
    let add_masked = r#"{"ir_version":1,"name":"add_masked","ops":[
        {"op":"addColumn","table":"people","column":"card","type":"text","nullable":true,
         "mask":{"kind":"first4","classification":"pci"}}
    ]}"#;
    all_ops.extend(
        apply_doc(&conn, &cfg, add_masked, &registry(&[("people", APP)]), Approval::None).await,
    );

    // LIVE introspection after both applies.
    let live = canonicalize(
        snapshot_schema(&conn, &cfg.project_schema).await.expect("introspect live schema"),
    );

    // (a) The live DB grew BOTH `_masked` siblings — proof the mask facet reached the DDL
    //     on the createTable path AND the addColumn path.
    assert!(
        has_masked_sibling(&live, "people", "ssn"),
        "the standalone-masked createTable column must grow a `ssn_masked` sibling on PG; \
         live people columns: {:?}",
        live.tables.get("people").map(|t| t.columns.iter().map(|c| &c.name).collect::<Vec<_>>())
    );
    assert!(
        has_masked_sibling(&live, "people", "card"),
        "the masked addColumn column must grow a `card_masked` sibling on PG; \
         live people columns: {:?}",
        live.tables.get("people").map(|t| t.columns.iter().map(|c| &c.name).collect::<Vec<_>>())
    );

    // (b) The OFFLINE fold of the SAME ops reproduces the live schema EXACTLY — proving the
    //     lower emits the siblings from the carried facets (fold == introspect keystone).
    let folded =
        canonicalize(fold_ops(&all_ops, SqlDialect::Postgres, &cfg.project_schema).expect("fold"));
    assert_eq!(
        folded.tables.get("people"),
        live.tables.get("people"),
        "offline fold must reproduce the live masked schema (siblings included)"
    );

    // (c) `fold_to_field_defs` recovers BOTH standalone masks on the FieldDefs (the
    //     gen-types / runtime fidelity source).
    let defs = fold_to_field_defs(&all_ops, SqlDialect::Postgres, &cfg.project_schema)
        .expect("fold_to_field_defs");
    let ssn_mask = defs["people"]["ssn"]
        .get("mask")
        .unwrap_or_else(|| panic!("ssn mask must be recovered: {:#}", defs["people"]["ssn"]));
    assert_eq!(ssn_mask.get("kind").and_then(|v| v.as_str()), Some("last4"));
    assert_eq!(ssn_mask.get("classification").and_then(|v| v.as_str()), Some("spi"));
    let card_mask = defs["people"]["card"]
        .get("mask")
        .unwrap_or_else(|| panic!("card mask must be recovered: {:#}", defs["people"]["card"]));
    assert_eq!(card_mask.get("kind").and_then(|v| v.as_str()), Some("first4"));
    assert_eq!(card_mask.get("classification").and_then(|v| v.as_str()), Some("pci"));

    teardown(&conn, &cfg).await;
}
