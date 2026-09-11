//! Phase F Stage 1 smoke test — proves the monorepo consumes the published
//! `zero-migrate` engine AND that the platform's native `compio-postgres` driver
//! applies a real IR envelope END-TO-END through the [`SqlSession`] seam.
//!
//! The flow, over a REAL Postgres on :5440:
//!   1. open a live `compio_postgres::Client` and wrap it in [`CompioPgSession`]
//!      (this crate's `driver::SqlSession` adapter);
//!   2. author a `zeroship_migrate` IR envelope (`createTable` + `addColumn`) and run
//!      it through the REAL fail-closed load gate + lower (`IrAuthor::load_and_lower`,
//!      Postgres dialect);
//!   3. construct a `PostgresBackend<CompioPgSession>` + `MigrationEngine` and apply
//!      the lowered migration over the adapter;
//!   4. assert the table, the added column, AND the journal row all exist via an
//!      INDEPENDENT query over the same seam.
//!
//! Requires PostgreSQL through the test overlay or `PG_TEST_URL`; missing
//! configuration or connectivity fails the test. Each run owns token-suffixed
//! metadata and project schemas.

use zeroship_migrate::driver::SqlSession;
use zeroship_migrate::{
    effective_policy_from_charter_toml, resolve_create_table_policy, Approval, EffectivePolicy,
    ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, MigrationEngine, MigrationIr,
};
// PG-shaped surfaces live in the vendor crate: the neutrality refactor moved
// `PostgresBackend`, the dialect id and the journal reader off the facade, and the
// dialect enum (`SqlDialect::Postgres`) became a `DialectId` const.
use zeroship_migrate_postgres::backend::journal_sql::applied as read_journal;
use zeroship_migrate_postgres::{PostgresBackend, DIALECT as POSTGRES};
use zeroship_migrate_server::session::CompioPgSession;

/// The vendor backends handed to every engine entry point. `zeroship-migrate` is the
/// composition root; a host takes the set rather than naming vendors itself.
const VENDORS: zeroship_migrate_backend::registry::VendorSet = zeroship_migrate::shipping_vendors();

/// The confined table-shape ceiling (the seven system columns + three indexes +
/// `["id"]` PK + `author_primary_key = "forbid"`) — a `RootCharter` document composed
/// into an `EffectivePolicy` via the engine's `effective_policy_from_charter_toml`.
/// (The old `PolicyProfile::confined()` is gone; the confined shape is now policy data.)
///
/// Only the GRANTS are written here, because only the grants are this fixture's
/// own: it pins `schema.cross_schema` to its throwaway project schema, which no
/// shipped ceiling does. The `[[inject]]` rule is the platform-wide fragment in
/// `policies/`, concatenated in at compile time, so this fixture cannot describe
/// a table shape the deployed server does not produce.
///
/// It could, and did. Until 2026-08-20 the rule was inlined here: `ddf636140`
/// added the created_at/updated_at/version DDL defaults to the two ceilings the
/// mirror gate then compared, and this fixture -- which its own doc comment calls
/// "the confined table-shape ceiling" -- kept the pre-fix shape for eleven days.
/// This test never inserts a row, so nothing went red; it simply stopped
/// exercising the shape a creator actually gets, which is the one thing a fixture
/// calling itself the confined ceiling is for.
const CONFINED_CEILING_TOML: &str = concat!(
    r#"policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["__PROJECT_SCHEMA__"] }

[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["__PROJECT_SCHEMA__"] }

[[grant]]
key = "schema.rename"
value = true
scope = { include = ["__PROJECT_SCHEMA__"] }

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"

"#,
    include_str!("../../../policies/confined-system-shape.inject.toml"),
);

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

fn confined_effective(project_schema: &str) -> EffectivePolicy {
    const PLACEHOLDER: &str = "\"__PROJECT_SCHEMA__\"";
    assert_eq!(CONFINED_CEILING_TOML.matches(PLACEHOLDER).count(), 3);
    let project_schema = serde_json::to_string(project_schema).expect("schema serializes");
    let charter = CONFINED_CEILING_TOML.replace(PLACEHOLDER, &project_schema);
    effective_policy_from_charter_toml(&charter).expect("confined charter composes")
}

fn cfg_for(tok: &str) -> (ExecutorConfig, EffectivePolicy) {
    let project_schema = format!("proj_{tok}");
    let effective = confined_effective(&project_schema);
    let mut c = ExecutorConfig::new(
        format!("{PROJECT}_{tok}"),
        project_schema,
        effective.clone(),
    );
    c.confinement.meta_schema = format!("meta_{tok}");
    (c, effective)
}

/// The env var gating the live-PG smoke test. Mirrors the standalone's suite gate.
/// The live `PostgreSQL` this target applies its migrations to.
///
/// # Panics
///
/// When neither `PG_TEST_URL` nor the test overlay names one, with the
/// provisioning command. It used to announce a skip, so a run against no
/// database reported the same green as one that had applied real DDL.
fn pg_url() -> String {
    zeroship_core::config::test_database_url()
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
            cfg.project_schema, cfg.confinement.meta_schema
        ))
        .await;
}

/// Resolve an IR envelope's `createTable` ops through the confined table-shape
/// policy (the platform's create-table policy) — the same normalisation the
/// SQLite IR-apply test uses before lowering. `addColumn` ops pass through
/// untouched.
fn resolved_envelope_json(
    raw: &str,
    effective: &EffectivePolicy,
    default_schema: &str,
) -> String {
    let ir: MigrationIr = serde_json::from_str(raw).expect("test IR parses");
    let resolved =
        resolve_create_table_policy(&ir, effective, default_schema).expect("test IR resolves");
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
    let url = pg_url();

    // (a) live compio client, (b) wrapped in this crate's SqlSession adapter.
    let session = CompioPgSession::connect(&url)
        .await
        .expect("connect compio session to test PG");
    let tok = token();
    let (cfg, effective) = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    // A single IR envelope with BOTH ops: createTable notes(title, body) then
    // addColumn notes.tag. Authored as a zeroship_migrate MigrationIr/envelope.
    let ir = resolved_envelope_json(
        r#"{"ir_version":1,"name":"create_notes_and_add_tag","ops":[
            {"op":"createTable","name":"notes","columns":[
                {"name":"title","type":"text","nullable":false},
                {"name":"body","type":"text"}
            ]},
            {"op":"addColumn","table":"notes","column":"tag","type":"text","nullable":true}
        ]}"#,
        &effective,
        &cfg.project_schema,
    );

    // (b→c) The REAL fail-closed gate + lower, Postgres dialect.
    let author = IrAuthor::new(VENDORS, &cfg.project_schema, APP, &POSTGRES, &effective);
    let migrations = author
        .load_and_lower(&ir, APP, &Default::default(), &LiveSchema::default())
        .expect("a valid IR envelope must lower on Postgres");
    assert!(!migrations.is_empty(), "lowering must yield migration(s)");

    // (c) PostgresBackend over the compio adapter + MigrationEngine.
    let engine = MigrationEngine::new(VENDORS);
    let guard_cfg = GuardConfig::from_policy(effective.clone(), POSTGRES, &cfg.project_schema);
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
    let applied = read_journal(&session, &cfg)
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
    let applied2 = read_journal(&session, &cfg)
        .await
        .expect("journal re-read");
    assert_eq!(
        applied2.len(),
        migrations.len(),
        "no duplicate journal rows on idempotent re-apply"
    );

    drop_schemas(&session, &cfg).await;
}
