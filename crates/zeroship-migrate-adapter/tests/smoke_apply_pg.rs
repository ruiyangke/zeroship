//! Phase F Stage 1 smoke test — proves the monorepo consumes the published
//! `zero-migrate` engine AND that the platform's native `compio-postgres` driver
//! applies a real IR envelope END-TO-END through the [`SqlSession`] seam.
//!
//! The flow, over a REAL Postgres on :5440:
//!   1. open a live `compio_postgres::Client` and wrap it in [`CompioPgSession`]
//!      (this crate's `driver::SqlSession` adapter);
//!   2. author a `zero_migrate` IR envelope (`createTable` + `addColumn`) and run
//!      it through the REAL fail-closed load gate + lower (`IrAuthor::load_and_lower`,
//!      Postgres dialect);
//!   3. construct a `PostgresBackend<CompioPgSession>` + `MigrationEngine` and apply
//!      the lowered migration over the adapter;
//!   4. assert the table, the added column, AND the journal row all exist via an
//!      INDEPENDENT query over the same seam.
//!
//! GATED behind `ZERO_MIGRATE_TEST_PG_URL` (a DSN on :5440): the test skips cleanly
//! when unset, so DB-free CI stays green. It runs in its OWN meta + project schema
//! (suffixed by a unique token) so the shared DB stays clean and re-runs are
//! independent.

use zero_migrate::driver::SqlSession;
use zero_migrate::{
    effective_policy_from_ceiling_toml, resolve_create_table_policy, Approval, ExecutorConfig,
    GuardConfig, IrAuthor, LiveSchema, MigrationEngine, MigrationIr, PostgresBackend, SqlDialect,
};
use zeroship_migrate_adapter::CompioPgSession;

/// The confined table-shape ceiling (the seven system columns + three indexes +
/// `["id"]` PK + `author_primary_key = "forbid"`) — a `RootCeiling` document composed
/// into an `EffectivePolicy` via the engine's `effective_policy_from_ceiling_toml`.
/// (The old `PolicyProfile::confined()` is gone; the confined shape is now policy data.)
const CONFINED_CEILING_TOML: &str = r#"policy_version = 1

[[grant]]
key = "schema.create_table"
value = true
scope = "all"

[[grant]]
key = "schema.rename"
value = true
scope = "all"

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"

[[inject]]
scope = "all"
mandatory = true
primary_key = ["id"]
author_primary_key = "forbid"
columns = [
  { name = "id",         type = "text",        nullable = false },
  { name = "created_at", type = "timestamptz", nullable = false },
  { name = "updated_at", type = "timestamptz", nullable = false },
  { name = "created_by", type = "text",        nullable = true  },
  { name = "updated_by", type = "text",        nullable = true  },
  { name = "version",    type = "integer",     nullable = false },
  { name = "deleted_at", type = "timestamptz", nullable = true  },
]
indexes = [
  { name = "ix_deleted_at", columns = ["deleted_at"] },
  { name = "ix_updated_at", columns = ["updated_at"] },
  { name = "ix_created_by", columns = ["created_by"] },
]
"#;

const PROJECT: &str = "prj_smoke";
const APP: &str = "app_smoke";

/// A unique token so the test gets isolated meta + project schemas in the shared DB.
fn token() -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid}_{nanos}")
}

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("{PROJECT}_{tok}"), format!("proj_{tok}"));
    c.pg.meta_schema = format!("meta_{tok}");
    c
}

/// The env var gating the live-PG smoke test. Mirrors the standalone's suite gate.
fn pg_url() -> Option<String> {
    std::env::var("ZERO_MIGRATE_TEST_PG_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

async fn ensure_project_schema(session: &CompioPgSession, cfg: &ExecutorConfig) {
    session
        .batch(&format!(
            "CREATE SCHEMA IF NOT EXISTS \"{}\"",
            cfg.project_schema
        ))
        .await
        .expect("create project schema");
}

async fn drop_schemas(session: &CompioPgSession, cfg: &ExecutorConfig) {
    let _ = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}

/// Resolve an IR envelope's `createTable` ops through the confined table-shape
/// policy (the platform's create-table policy) — the same normalisation the
/// SQLite IR-apply test uses before lowering. `addColumn` ops pass through
/// untouched.
fn resolved_envelope_json(raw: &str) -> String {
    let ir: MigrationIr = serde_json::from_str(raw).expect("test IR parses");
    let confined =
        effective_policy_from_ceiling_toml(CONFINED_CEILING_TOML).expect("confined ceiling composes");
    let resolved = resolve_create_table_policy(&ir, &confined).expect("test IR resolves");
    serde_json::to_string(&resolved).expect("resolved test IR serializes")
}

/// Does `schema.table` exist? (independent probe over the seam)
async fn table_exists(session: &CompioPgSession, schema: &str, table: &str) -> bool {
    let row = session
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_schema = $1 AND table_name = $2) AS present",
            &[schema.into(), table.into()],
        )
        .await
        .expect("table_exists probe");
    row.try_get::<_, bool>("present").expect("decode present")
}

/// Does `schema.table.column` exist? (independent probe over the seam)
async fn column_exists(session: &CompioPgSession, schema: &str, table: &str, column: &str) -> bool {
    let row = session
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 AND column_name = $3) AS present",
            &[schema.into(), table.into(), column.into()],
        )
        .await
        .expect("column_exists probe");
    row.try_get::<_, bool>("present").expect("decode present")
}

#[compio::test]
async fn ir_envelope_lowers_and_applies_over_native_compio_seam() {
    let Some(url) = pg_url() else {
        eprintln!(
            "skipping Phase F smoke: ZERO_MIGRATE_TEST_PG_URL unset \
             (set it to a DSN on :5440 to run)"
        );
        return;
    };

    // (a) live compio client, (b) wrapped in this crate's SqlSession adapter.
    let session = CompioPgSession::connect(&url)
        .await
        .expect("connect compio session to test PG");
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    // A single IR envelope with BOTH ops: createTable notes(title, body) then
    // addColumn notes.tag. Authored as a zero_migrate MigrationIr/envelope.
    let ir = resolved_envelope_json(
        r#"{"ir_version":1,"name":"create_notes_and_add_tag","ops":[
            {"op":"createTable","name":"notes","columns":[
                {"name":"title","type":"text","nullable":false},
                {"name":"body","type":"text"}
            ]},
            {"op":"addColumn","table":"notes","column":"tag","type":"text","nullable":true}
        ]}"#,
    );

    // (b→c) The REAL fail-closed gate + lower, Postgres dialect.
    let author = IrAuthor::new(&cfg.project_schema, APP, SqlDialect::Postgres);
    let migrations = author
        .load_and_lower(&ir, APP, &Default::default(), &LiveSchema::default())
        .expect("a valid IR envelope must lower on Postgres");
    assert!(!migrations.is_empty(), "lowering must yield migration(s)");

    // (c) PostgresBackend over the compio adapter + MigrationEngine.
    let engine = MigrationEngine::new();
    let guard_cfg = GuardConfig::confined(cfg.project_schema.clone());
    let plan = engine.plan(&migrations, &guard_cfg);
    assert!(
        plan.denied.is_empty(),
        "no denials on a clean IR set: {:?}",
        plan.denied
    );

    let backend = PostgresBackend::new_generic(&session);

    // (d) apply the lowered IR envelope through the NATIVE compio seam.
    let outcome = engine
        .apply(&plan, Approval::None, &backend, &cfg, "phase-f-smoke")
        .await
        .expect("apply the lowered IR over the native compio PG seam");
    assert!(!outcome.applied.is_empty(), "the IR migration must apply");

    // (e) INDEPENDENT assertions: the table, the added column, and the journal row
    // all exist — proving the native path drove real DDL + journaling end-to-end.
    assert!(
        table_exists(&session, &cfg.project_schema, "notes").await,
        "the IR-created 'notes' table must exist on real PG"
    );
    assert!(
        column_exists(&session, &cfg.project_schema, "notes", "title").await,
        "the createTable 'title' column must exist"
    );
    assert!(
        column_exists(&session, &cfg.project_schema, "notes", "tag").await,
        "the addColumn 'tag' column must exist"
    );

    // The journal recorded EVERY lowered migration, readable back over the seam.
    // (The confined table-shape policy injects system-column/index migrations, so
    // the envelope lowers to `migrations.len()` migrations, not one — the journal
    // row count must match the number applied.)
    let applied = zero_migrate::applied(&session, &cfg)
        .await
        .expect("journal read over the seam");
    assert_eq!(
        applied.len(),
        migrations.len(),
        "one journal row per lowered migration"
    );

    // Idempotent re-run: no-op, no second journal row.
    let plan2 = engine.plan(&migrations, &guard_cfg);
    let out2 = engine
        .apply(&plan2, Approval::None, &backend, &cfg, "phase-f-smoke")
        .await
        .expect("idempotent re-apply");
    assert!(out2.is_noop(), "second apply is a no-op");
    let applied2 = zero_migrate::applied(&session, &cfg)
        .await
        .expect("journal re-read");
    assert_eq!(
        applied2.len(),
        migrations.len(),
        "no duplicate journal rows on idempotent re-apply"
    );

    drop_schemas(&session, &cfg).await;
}
