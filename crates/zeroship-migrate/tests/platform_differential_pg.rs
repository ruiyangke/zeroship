//! MED #3 (spec §10 PR0 test 7, "mandatory") — the Platform Flyway-mode
//! DIFFERENTIAL APPLY test on REAL Postgres (`:5440`).
//!
//! PR0 generalized the platform loader: it used to read a flat `Vec<Migration>`
//! via `load_dir_migrations`; it now reads `Vec<AppliedPlan>` via `load_dir` and
//! the production platform runner projects each single-step plan back to its
//! `&Migration` through `AppliedPlan::single_step_migration()` (the
//! `load_dir_flat` facade). The structural-identity argument (`load_dir ==
//! load_dir_migrations().map(single_step)`, and `single_step_migration()` returns
//! that same `&Migration`) is sound — but the spec demanded the differential test,
//! not the argument.
//!
//! This drives the SAME generated Flyway changelog through BOTH loaders, applies
//! each to its OWN fresh schema through the SAME engine path
//! (`engine.plan(&migs).apply(...)`), and asserts:
//!   - byte-identical JOURNAL TRACE (the `(version, name, checksum)` rows in
//!     event order), and
//!   - byte-identical applied SCHEMA fingerprint (every table + column + type in
//!     the project schema, ordered).
//!
//! The changelog is GENERATED (a deterministic dependency-safe Flyway dir), so the
//! test does NOT depend on the untracked `db/migrations` build artifact.

use compio_postgres::Client;
use zeroship_migrate::{
    model::migration::Migration, provision_migrator, apply::role::deprovision_migrator, Approval, ExecutorConfig,
    GuardConfig, MigrationEngine,
};

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
    let role = zeroship_migrate::migrator_role_name(&c.project_id).unwrap();
    c.with_migrator_role(role)
}

async fn setup(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS \"{}\"",
        cfg.project_schema
    ))
    .await
    .expect("create project schema");
    provision_migrator(conn, cfg)
        .await
        .expect("provision migrator role");
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

/// The journal "trace": `(version, name, checksum)` for every completed apply, in
/// event order — what the differential asserts byte-identical across loaders.
async fn journal_trace(conn: &Client, cfg: &ExecutorConfig) -> Vec<(String, String, String)> {
    let rows = conn
        .query(
            &format!(
                "SELECT version, name, checksum FROM \"{}\".schema_migrations \
                 WHERE phase = 'completed' AND event_kind = 'applied' ORDER BY event_seq ASC",
                cfg.pg.meta_schema
            ),
            &[],
        )
        .await
        .expect("query journal trace");
    rows.iter()
        .map(|r| {
            (
                r.get::<_, String>(0),
                r.get::<_, String>(1),
                r.get::<_, String>(2),
            )
        })
        .collect()
}

/// The applied-schema fingerprint: every `(table, column, type, nullable)` in the
/// project schema, ordered — the structural equivalent of a normalized
/// `pg_dump --schema-only`, without the pg_dump shell fragility.
async fn schema_fingerprint(conn: &Client, cfg: &ExecutorConfig) -> Vec<String> {
    let rows = conn
        .query(
            "SELECT table_name, column_name, data_type, is_nullable \
             FROM information_schema.columns WHERE table_schema = $1 \
             ORDER BY table_name, ordinal_position",
            &[&cfg.project_schema],
        )
        .await
        .expect("query schema fingerprint");
    rows.iter()
        .map(|r| {
            format!(
                "{}.{} {} {}",
                r.get::<_, String>(0),
                r.get::<_, String>(1),
                r.get::<_, String>(2),
                r.get::<_, String>(3),
            )
        })
        .collect()
}

/// Generate a deterministic, dependency-safe Flyway changelog dir: `V0001` creates
/// a base table, each later `V<NNNN>` ADDs a column to it (so every migration
/// actually applies in order, against a real DB, like the platform changelog).
///
/// The DDL is deliberately UNQUALIFIED (no `<schema>.` prefix) so the changelog
/// CONTENT — hence each migration's checksum — is identical regardless of which
/// project schema it is applied into; the platform profile pins the search_path
/// to the project schema, so the unqualified DDL resolves there. This keeps the
/// differential a test of the LOADER, not of incidental schema-name differences.
fn gen_flyway_dir(seed: u64, n: u64) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("V0001__create_base.sql"),
        format!("CREATE TABLE base_{seed} (id bigint PRIMARY KEY)"),
    )
    .expect("write V0001");
    for v in 2..=n {
        std::fs::write(
            dir.path().join(format!("V{v:04}__add_col_{v}.sql")),
            format!("ALTER TABLE base_{seed} ADD COLUMN c{v} text"),
        )
        .unwrap_or_else(|e| panic!("write V{v:04}: {e}"));
    }
    dir
}

/// Apply a `Vec<Migration>` through the SAME engine path the platform runner uses
/// (`engine.plan(&migs, guard).apply(...)`), returning the journal trace + schema
/// fingerprint of the resulting schema.
async fn apply_and_fingerprint(
    conn: &Client,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
) -> (Vec<(String, String, String)>, Vec<String>) {
    let engine = MigrationEngine::new();
    let guard = GuardConfig::confined(cfg.project_schema.clone());
    let plan = engine.plan(migrations, &guard);
    engine
        .apply(
            &plan,
            Approval::Approved,
            &zeroship_migrate::PostgresBackend::new(conn),
            cfg,
            "platform-migrate",
        )
        .await
        .expect("platform changelog applies");
    (
        journal_trace(conn, cfg).await,
        schema_fingerprint(conn, cfg).await,
    )
}

#[compio::test]
async fn platform_flyway_loader_generalization_is_apply_identical() {
    let conn = pg().await;

    for seed in 0u64..4 {
        let n = 2 + seed % 4; // 2..5 migrations
        // ONE generated changelog dir (schema-agnostic content), applied via BOTH
        // loaders to two fresh schemas.
        let dir = gen_flyway_dir(seed, n);

        // Path A — the POST-change platform loader: `load_dir` →
        // `single_step_migration()` (exactly what the production `load_dir_flat`
        // does in `command::runner::run_migrate_pg`).
        let cfg_a = cfg_for(&token());
        setup(&conn, &cfg_a).await;
        let plans = zeroship_migrate::load_dir(dir.path()).expect("load_dir (plans)");
        let migs_a: Vec<Migration> = plans
            .iter()
            .map(|p| {
                p.single_step_migration()
                    .expect("platform .sql lowers to a single Ddl step")
                    .clone()
            })
            .collect();
        let (trace_a, schema_a) = apply_and_fingerprint(&conn, &cfg_a, &migs_a).await;
        teardown(&conn, &cfg_a).await;

        // Path B — the PRE-change flat loader: `load_dir_migrations` directly, on
        // the SAME changelog dir.
        let cfg_b = cfg_for(&token());
        setup(&conn, &cfg_b).await;
        let migs_b: Vec<Migration> =
            zeroship_migrate::load_dir_migrations(dir.path()).expect("load_dir_migrations (flat)");
        let (trace_b, schema_b) = apply_and_fingerprint(&conn, &cfg_b, &migs_b).await;
        teardown(&conn, &cfg_b).await;

        // The two loaders produce the SAME migration set, applied identically:
        // byte-identical journal trace + byte-identical applied schema.
        assert_eq!(
            migs_a.len(),
            n as usize,
            "seed {seed}: every generated .sql lowered to exactly one migration"
        );
        assert_eq!(
            trace_a, trace_b,
            "seed {seed}: the plan-projected loader and the flat loader must journal \
             a BYTE-IDENTICAL trace (the platform loader generalization is apply-neutral)"
        );
        assert_eq!(
            schema_a, schema_b,
            "seed {seed}: both loaders must produce a BYTE-IDENTICAL applied schema"
        );
        // And the schema is non-trivial (the changelog actually built something).
        assert!(
            schema_a.len() >= n as usize,
            "seed {seed}: the applied schema has at least one column per migration"
        );
    }
}
