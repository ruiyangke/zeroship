#![cfg(feature = "zsv8")]
//! PR4 test #1 (PG leg) + #2 — the build/new path applied on REAL Postgres :5440,
//! and the `generate` autogenerate flow (re-diff to zero + autogenerate parity +
//! machine-readable TODO-backfill marker).
//!
//! Requires `zero_migrate_test` on :5440 (the existing `*_pg.rs` convention:
//! connect directly + `.expect`; run only when :5440 is up).
//!
//! Faithful: the REAL sandboxed recorder child + the REAL engine apply + REAL PG
//! introspection + the REAL declarative differ for the re-diff-to-zero assertion.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use compio_postgres::Client;
use zero_migrate::apply::drift::snapshot_schema;
use zero_migrate::render::declarative::{desired_snapshot, DeclarativeAuthor, DesiredSchema};
use zero_migrate::{
    Approval, ExecutorConfig, GuardConfig, IndexElement, IrAuthor, LiveSchema, MigrationEngine,
    SqlDialect,
};
use zero_migrate::frontend::recorder_service::recorder_child_path;
use zero_migrate::frontend::{
    build_migrations, eval_schema_to_ir, generate_ops, RecordVia, ResourceBudget,
};

const APP: &str = "app_pr4_pg";

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zero_migrate_test";

fn dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| DEFAULT_DSN.to_string())
}

async fn pg() -> Client {
    let (client, conn) = compio_postgres::connect(&dsn(), compio_postgres::NoTls)
        .await
        .expect("connect to zero_migrate_test on :5440");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

fn unique_schema(tag: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("pr4_{tag}_{}_{nanos}_{n}", std::process::id())
}

fn assert_child_built() {
    let bin = recorder_child_path();
    assert!(
        bin.exists(),
        "recorder child binary missing at {} — run `cargo build -p zero-migrate --bins` first",
        bin.display()
    );
}

// A simple, portable creator schema (text/number columns only — generate's
// structural-delta scope). Resolves `@zeroship/db` via the eval graph.
const SIMPLE_SCHEMA: &str = r#"
import { t } from "@zeroship/db";
const widgets = {
  label: t.string().required(),
  qty: t.number(),
};
export default { schema: { widgets } };
"#;

// ---------------------------------------------------------------------------
// Test #1 (PG leg): new → edit → build → apply on real PG.
// ---------------------------------------------------------------------------
#[compio::test]
async fn new_edit_build_apply_on_pg() {
    assert_child_built();
    let conn = pg().await;
    let schema = unique_schema("apply");
    conn.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\""))
        .await
        .expect("create schema");

    let mig_dir = tempfile::tempdir().unwrap();
    let stem = "20240617150000_make_widgets";
    let ts = r#"
import { table, t } from "@zeroship/migrate";
export function up() {
  table("widgets").create({
    columns: {
      label: t.text().notNull(),
      status: t.text().notNull().default("new"),
    },
  });
  table("widgets").column("qty").add({ type: t.int() });
}
"#;
    fs::write(mig_dir.path().join(format!("{stem}.ts")), ts.as_bytes()).unwrap();

    // build via the LOCAL sandboxed child → transient canonical IR.
    let outcome = build_migrations(&zero_migrate_host::ZeroMigrateHost, 
        mig_dir.path(),
        APP,
        &RecordVia::Local {
            budget: ResourceBudget::default(),
        },
    )
    .expect("build records transient IR");
    let committed = String::from_utf8(outcome.migrations[0].committed_bytes.clone()).unwrap();
    assert!(
        !mig_dir.path().join(format!("{stem}.ir.json")).exists(),
        "build must not write a committed .ir.json"
    );

    // Apply the transient `.ir.json` through the REAL gate + lower on PG.
    let author = IrAuthor::new(&schema, APP, SqlDialect::Postgres);
    let migrations = author
        .load_and_lower(&committed, APP, &BTreeMap::new(), &LiveSchema::default(), None)
        .expect("transient .ir.json lowers on PG");
    let engine = MigrationEngine::new();
    let plan = engine.plan(&migrations, &GuardConfig::confined(schema.clone()));
    assert!(plan.denied.is_empty(), "no denials: {:?}", plan.denied);
    let applied = engine
        .apply(
            &plan,
            Approval::None,
            &zero_migrate::PostgresBackend::new(&conn),
            &ExecutorConfig::new(&schema, &schema),
            "deploy",
        )
        .await
        .expect("apply on PG");
    assert!(!applied.applied.is_empty());

    // The table + columns exist in the live schema.
    let rows = conn
        .query(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_schema=$1 AND table_name='widgets' AND column_name IN ('label','status','qty')",
            &[&schema],
        )
        .await
        .expect("introspect widgets columns");
    assert_eq!(rows.len(), 3, "the createTable + addColumn columns must exist on PG");

    conn.batch_execute(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .await
        .ok();
}

// ---------------------------------------------------------------------------
// Test #2: generate round-trips a t.* schema diff into an APPLYABLE IR that
// re-diffs to ZERO; the emitted `.ts` records to the same checksum as the
// directly-derived IR (autogenerate parity); a non-null-no-default column add
// emits the machine-readable TODO-backfill marker.
// ---------------------------------------------------------------------------
#[compio::test]
async fn generate_redifs_to_zero_and_parity_and_todo_marker() {
    assert_child_built();
    let conn = pg().await;
    let schema = unique_schema("gen");
    conn.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\""))
        .await
        .expect("create schema");

    // 1. Baseline empty schema → desired snapshot from the t.* schema.
    let descriptors = eval_schema_to_ir(SIMPLE_SCHEMA, APP).expect("eval schema");
    let desired = desired_snapshot(&schema, &descriptors).expect("desired snapshot");
    let live0 = snapshot_schema(&conn, &schema).await.expect("introspect live (empty)");

    let gen = generate_ops("create_widgets", APP, &desired, &live0).expect("generate ops");
    assert!(!gen.is_empty, "a fresh schema must generate a non-empty migration");

    // 2. Apply the generated `.ir.json` (the source of truth) on PG.
    let mut ir_bytes = serde_json::to_string_pretty(&gen.ir).unwrap();
    ir_bytes.push('\n');
    let author = IrAuthor::new(&schema, APP, SqlDialect::Postgres);
    let migrations = author
        .load_and_lower(&ir_bytes, APP, &BTreeMap::new(), &LiveSchema::default(), None)
        .expect("generated IR lowers on PG");
    let engine = MigrationEngine::new();
    let plan = engine.plan(&migrations, &GuardConfig::confined(schema.clone()));
    assert!(plan.denied.is_empty(), "no denials: {:?}", plan.denied);
    engine
        .apply(
            &plan,
            Approval::None,
            &zero_migrate::PostgresBackend::new(&conn),
            &ExecutorConfig::new(&schema, &schema),
            "gen",
        )
        .await
        .expect("apply generated IR on PG");

    // 3. RE-DIFF to zero: introspect the now-applied live schema and diff against
    //    the SAME desired snapshot — the plan must be EMPTY.
    let live1 = snapshot_schema(&conn, &schema).await.expect("introspect live (applied)");
    let ownership: std::collections::HashMap<String, String> = live1
        .tables
        .keys()
        .map(|t| (t.clone(), APP.to_string()))
        .collect();
    let differ = DeclarativeAuthor::new(&schema, APP);
    let replan = differ
        .diff(&desired, &live1, &ownership, &[])
        .expect("re-diff");
    assert!(
        replan.all_migrations().is_empty(),
        "the generated migration must re-diff to ZERO; residual: {:?}",
        replan.all_migrations().iter().map(|m| &m.up).collect::<Vec<_>>()
    );

    // 4. AUTOGENERATE PARITY: record the emitted `.ts` through the recorder and
    //    assert its IR checksum == the directly-derived IR's checksum.
    let mig_dir = tempfile::tempdir().unwrap();
    let stem = "20240617150000_create_widgets";
    fs::write(mig_dir.path().join(format!("{stem}.ts")), gen.ts_body.as_bytes()).unwrap();
    let built = build_migrations(&zero_migrate_host::ZeroMigrateHost, 
        mig_dir.path(),
        APP,
        &RecordVia::Local {
            budget: ResourceBudget::default(),
        },
    )
    .expect("record the emitted .ts");
    // The directly-derived IR's typed checksum.
    let derived_checksum = direct_checksum(&gen.ir);
    assert_eq!(
        built.migrations[0].checksum, derived_checksum,
        "the emitted .ts must record to the SAME checksum as the directly-derived IR (autogenerate parity)"
    );

    conn.batch_execute(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .await
        .ok();
}

// A schema carrying a UNIQUE field → a unique INDEX (the MED-1 surface): generate
// must SYNTHESIZE the index as a standalone createIndex op so the migration
// re-diffs to ZERO. Pre-fix the index was silently dropped and the re-diff churned.
const INDEXED_SCHEMA: &str = r#"
import { t } from "@zeroship/db";
const accounts = {
  email: t.string().required().unique(),
  handle: t.string(),
};
export default { schema: { accounts } };
"#;

// ---------------------------------------------------------------------------
// MED-1: generate of an INDEX-bearing schema re-diffs to ZERO on real PG (the
// synthesized createIndex actually applies; the desired index is not dropped).
// ---------------------------------------------------------------------------
#[compio::test]
async fn generate_index_bearing_schema_redifs_to_zero_on_pg() {
    assert_child_built();
    let conn = pg().await;
    let schema = unique_schema("genidx");
    conn.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\""))
        .await
        .expect("create schema");

    let descriptors = eval_schema_to_ir(INDEXED_SCHEMA, APP).expect("eval schema");
    let desired = desired_snapshot(&schema, &descriptors).expect("desired snapshot");
    let live0 = snapshot_schema(&conn, &schema).await.expect("introspect live (empty)");

    let gen = generate_ops("create_accounts", APP, &desired, &live0).expect("generate ops");
    assert!(!gen.is_empty);
    // The unique index over `email` MUST be synthesized as a standalone createIndex.
    use zero_migrate::model::ir::Op;
    assert!(
        gen.ir.ops.iter().any(|o| matches!(
            o,
            Op::CreateIndex { columns, .. }
                if columns
                    == &vec![IndexElement::Column {
                        name: "email".to_string(),
                        order: None,
                        opclass: None,
                        collation: None,
                    }]
        )),
        "the unique-field index must be synthesized as a createIndex op; ops: {:?}",
        gen.ir.ops
    );

    // Apply the generated IR on PG.
    let mut ir_bytes = serde_json::to_string_pretty(&gen.ir).unwrap();
    ir_bytes.push('\n');
    let author = IrAuthor::new(&schema, APP, SqlDialect::Postgres);
    let migrations = author
        .load_and_lower(&ir_bytes, APP, &BTreeMap::new(), &LiveSchema::default(), None)
        .expect("generated IR lowers on PG");
    let engine = MigrationEngine::new();
    let plan = engine.plan(&migrations, &GuardConfig::confined(schema.clone()));
    assert!(plan.denied.is_empty(), "no denials: {:?}", plan.denied);
    engine
        .apply(
            &plan,
            Approval::None,
            &zero_migrate::PostgresBackend::new(&conn),
            &ExecutorConfig::new(&schema, &schema),
            "genidx",
        )
        .await
        .expect("apply generated IR on PG");

    // RE-DIFF to zero: the synthesized index made the live schema match desired.
    let live1 = snapshot_schema(&conn, &schema).await.expect("introspect live (applied)");
    let ownership: std::collections::HashMap<String, String> = live1
        .tables
        .keys()
        .map(|t| (t.clone(), APP.to_string()))
        .collect();
    let differ = DeclarativeAuthor::new(&schema, APP);
    let replan = differ.diff(&desired, &live1, &ownership, &[]).expect("re-diff");
    assert!(
        replan.all_migrations().is_empty(),
        "the index-bearing generated migration must re-diff to ZERO; residual: {:?}",
        replan.all_migrations().iter().map(|m| &m.up).collect::<Vec<_>>()
    );

    conn.batch_execute(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .await
        .ok();
}

/// The TODO-backfill marker is emitted for a NON-NULL column add with no default.
#[compio::test]
async fn generate_emits_machine_readable_backfill_todo() {
    use zero_migrate::{ColumnSnapshot, SchemaSnapshot, TableSnapshot};

    // Desired: a table with a NON-NULL `priority` column and no default.
    // Live: the SAME table WITHOUT `priority` (so it's an addColumn).
    let mut desired_tbl = TableSnapshot {
        columns: vec![
            ColumnSnapshot { name: "id".into(), data_type: "text".into(), nullable: false, ..Default::default() },
            ColumnSnapshot { name: "priority".into(), data_type: "integer".into(), nullable: false, ..Default::default() },
        ],
        indexes: vec![],
        constraints: vec![],
        runtime_options: Default::default(),

    partition_by: None,

    comment: None,
        stored_create_sql: None,
    };
    desired_tbl.columns.sort_by(|a, b| a.name.cmp(&b.name));
    let mut dsnap = SchemaSnapshot::default();
    dsnap.tables.insert("tasks".into(), desired_tbl);
    let desired = DesiredSchema {
        snapshot: dsnap,
        ownership: [("tasks".to_string(), APP.to_string())].into_iter().collect(),
        sqlite_schemas: Default::default(),
    };

    let mut live_tbl = TableSnapshot {
        columns: vec![ColumnSnapshot {
            name: "id".into(),
            data_type: "text".into(),
            nullable: false,
            ..Default::default()
        }],
        indexes: vec![],
        constraints: vec![],
        runtime_options: Default::default(),

    partition_by: None,

    comment: None,
        stored_create_sql: None,
    };
    live_tbl.columns.sort_by(|a, b| a.name.cmp(&b.name));
    let mut live = SchemaSnapshot::default();
    live.tables.insert("tasks".into(), live_tbl);

    let gen = generate_ops("add_priority", APP, &desired, &live).expect("generate");
    assert!(
        gen.todos.iter().any(|t| t.contains("@zeroship-todo backfill")
            && t.contains("tasks.priority")),
        "a NON-NULL no-default column add must emit a machine-readable backfill TODO; got {:?}",
        gen.todos
    );
    // The marker is also embedded in the emitted `.ts`.
    assert!(
        gen.ts_body.contains("@zeroship-todo backfill"),
        "the backfill marker must be embedded in the emitted .ts"
    );
}

/// The directly-derived IR's typed-value checksum (the `Checksum::of_ir` anchor).
fn direct_checksum(ir: &zero_migrate::MigrationIr) -> String {
    use zero_migrate::model::ir::CanonicalOpList;
    use zero_migrate::{Checksum, MigrationFlags};
    Checksum::of_ir(
        &CanonicalOpList(&ir.ops),
        &MigrationFlags::default(),
        &ir.owner_app,
        &[],
        &[],
        &ir.preconditions,
    )
    .as_str()
    .to_string()
}

// silence unused import on non-PG builds (none here; PG is always compiled)
#[allow(dead_code)]
fn _unused(_: &Path, _: &BTreeSet<String>) {}
