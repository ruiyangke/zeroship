//! Faithful PG tests for the `zeroship-migrate` CLI runner functions (Phase 3,
//! design §9). Drives the REAL `guard::platform_runner::run_*` functions (not a
//! spawned process, not a shim) against a real Postgres on :5440.
//!
//! Each test uses a UNIQUE platform schema + meta schema (token-suffixed) so it
//! never collides with the real `zeroship` schema or with a concurrent test. The
//! Platform profile is proven by including a privileged op (`CREATE SCHEMA` +
//! `GRANT`) that the Confined guard would DENY but Platform admits.

use std::path::{Path, PathBuf};

use compio_postgres::Client;
use zeroship_migrate::guard::platform_runner::{
    run_migrate, run_status, run_validate, RunConfig, RunProfile, RunReport,
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

/// A throwaway migration directory under the OS temp dir, with the given files
/// written. Returns the dir path; dropping the guard removes the tree.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tok: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("zsmig_cli_{tok}"));
        std::fs::create_dir_all(&dir).expect("create temp migration dir");
        Self(dir)
    }
    fn write(&self, name: &str, body: &str) {
        std::fs::write(self.0.join(name), body).expect("write migration file");
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A Platform [`RunConfig`] over a UNIQUE schema/meta so it never touches the real
/// `zeroship` schema. The allowlist is `[<schema>, public]` so the privileged ops
/// (CREATE SCHEMA / GRANT against the unique schema) are in-allowlist.
fn platform_cfg(dir: &Path, schema: &str, meta: &str, yes: bool) -> RunConfig {
    RunConfig {
        dir: dir.to_path_buf(),
        database_url: dsn(),
        profile: RunProfile::Platform,
        project_id: format!("platform_test_{schema}"),
        project_schema: schema.to_string(),
        schemas: vec![schema.to_string(), "public".to_string()],
        extensions: Vec::new(),
        meta_schema: meta.to_string(),
        yes,
        statement_timeout: std::time::Duration::from_secs(60),
        lock_timeout: std::time::Duration::from_secs(30),
    }
}

async fn drop_schemas(conn: &Client, schema: &str, meta: &str) {
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{schema}\" CASCADE; DROP SCHEMA IF EXISTS \"{meta}\" CASCADE;"
        ))
        .await;
}

async fn table_exists(conn: &Client, schema: &str, table: &str) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.tables WHERE table_schema = $1 AND table_name = $2",
            &[&schema, &table],
        )
        .await
        .expect("query table existence");
    !rows.is_empty()
}

// ---------------------------------------------------------------------------
// run_migrate — applies a PRIVILEGED platform set; idempotent re-run; status.
// ---------------------------------------------------------------------------

#[compio::test]
async fn run_migrate_applies_privileged_platform_set_then_idempotent() {
    let conn = pg().await;
    let tok = token();
    let schema = format!("plat_{tok}");
    let meta = format!("platmeta_{tok}");
    drop_schemas(&conn, &schema, &meta).await;

    let dir = TempDir::new(&tok);
    // V0001: create the platform schema + a table (CREATE SCHEMA is DENIED under
    // Confined — proves the run is Platform).
    dir.write(
        "V0001__init.sql",
        &format!(
            "CREATE SCHEMA IF NOT EXISTS \"{schema}\";\n\
             CREATE TABLE \"{schema}\".widgets (id int PRIMARY KEY);"
        ),
    );
    // V0002: a GRANT (privilege management — DENIED under Confined, ALLOW Platform).
    dir.write(
        "V0002__grant.sql",
        &format!("GRANT USAGE ON SCHEMA \"{schema}\" TO PUBLIC;"),
    );

    let cfg = platform_cfg(dir.path(), &schema, &meta, false);

    // First apply: both files apply (additive ⇒ no --yes needed).
    let report = run_migrate(&cfg).await.expect("platform migrate applies");
    match report {
        RunReport::Migrate(outcome) => {
            assert_eq!(outcome.applied.len(), 2, "both V0001 + V0002 applied");
            assert!(!outcome.is_noop());
        }
        other => panic!("expected Migrate report, got {other:?}"),
    }
    assert!(
        table_exists(&conn, &schema, "widgets").await,
        "the platform schema + table materialized"
    );

    // Second apply: idempotent no-op.
    let report2 = run_migrate(&cfg).await.expect("idempotent re-run");
    match report2 {
        RunReport::Migrate(outcome) => assert!(outcome.is_noop(), "re-run is a no-op"),
        other => panic!("expected Migrate report, got {other:?}"),
    }

    // Status shows both applied, none pending.
    let st = run_status(&cfg).await.expect("status reads journal");
    match st {
        RunReport::Status(status) => {
            assert_eq!(status.applied.len(), 2, "two migrations applied");
            assert!(status.pending.is_empty(), "nothing pending");
        }
        other => panic!("expected Status report, got {other:?}"),
    }

    drop_schemas(&conn, &schema, &meta).await;
}

// ---------------------------------------------------------------------------
// H1 destructive gate — a destructive file is refused without --yes, applied
// with --yes. Applied through the FULL runner path (not just the decision fn).
// ---------------------------------------------------------------------------

#[compio::test]
async fn run_migrate_destructive_gate_refuses_without_yes_then_applies_with_yes() {
    let conn = pg().await;
    let tok = token();
    let schema = format!("platd_{tok}");
    let meta = format!("platdmeta_{tok}");
    drop_schemas(&conn, &schema, &meta).await;

    // First, materialize the schema + a table to drop, via a non-destructive apply.
    let setup = TempDir::new(&format!("{tok}_setup"));
    setup.write(
        "V0001__init.sql",
        &format!(
            "CREATE SCHEMA IF NOT EXISTS \"{schema}\";\n\
             CREATE TABLE \"{schema}\".doomed (id int PRIMARY KEY);"
        ),
    );
    let setup_cfg = platform_cfg(setup.path(), &schema, &meta, false);
    run_migrate(&setup_cfg).await.expect("setup applies");
    assert!(table_exists(&conn, &schema, "doomed").await);

    // Now a destructive migration: DROP TABLE. Same dir as setup PLUS V0002.
    setup.write(
        "V0002__drop.sql",
        &format!("DROP TABLE \"{schema}\".doomed;"),
    );

    // Without --yes ⇒ refused, nothing applied (the table survives).
    let cfg_no_yes = platform_cfg(setup.path(), &schema, &meta, false);
    let err = run_migrate(&cfg_no_yes)
        .await
        .expect_err("destructive plan without --yes must be refused");
    assert!(
        format!("{err}").contains("DESTRUCTIVE"),
        "refusal mentions the destructive gate: {err}"
    );
    assert!(
        table_exists(&conn, &schema, "doomed").await,
        "nothing applied — the table still exists"
    );

    // With --yes ⇒ applies; the table is dropped.
    let cfg_yes = platform_cfg(setup.path(), &schema, &meta, true);
    let report = run_migrate(&cfg_yes)
        .await
        .expect("destructive plan with --yes applies");
    match report {
        RunReport::Migrate(outcome) => {
            assert_eq!(outcome.applied.len(), 1, "only the destructive V0002 was pending");
        }
        other => panic!("expected Migrate report, got {other:?}"),
    }
    assert!(
        !table_exists(&conn, &schema, "doomed").await,
        "with --yes the destructive drop applied"
    );

    drop_schemas(&conn, &schema, &meta).await;
}

// ---------------------------------------------------------------------------
// run_validate — NO DDL on the REAL DB (journal + schema unchanged after).
// ---------------------------------------------------------------------------

#[compio::test]
async fn run_validate_does_no_ddl_on_the_real_db() {
    let conn = pg().await;
    let tok = token();
    let schema = format!("platv_{tok}");
    let meta = format!("platvmeta_{tok}");
    drop_schemas(&conn, &schema, &meta).await;

    let dir = TempDir::new(&tok);
    dir.write(
        "V0001__init.sql",
        &format!(
            "CREATE SCHEMA IF NOT EXISTS \"{schema}\";\n\
             CREATE TABLE \"{schema}\".only_on_shadow (id int PRIMARY KEY);"
        ),
    );
    let cfg = platform_cfg(dir.path(), &schema, &meta, false);

    let report = run_validate(&cfg).await.expect("validate dry-runs");
    match report {
        RunReport::Validate(v) => {
            // The dry-run RAN on the shadow (one per-migration result recorded),
            // and the REAL journal shows no drift on a fresh DB. (Whether the
            // privileged CREATE SCHEMA *applies* on the shadow depends on the
            // shadow's NOSUPERUSER migrator-role model — a §9 deviation noted in
            // the report; the load-bearing claim here is NO DDL on the real DB.)
            assert_eq!(
                v.dry_run.per_migration.len(),
                1,
                "the dry-run exercised the one migration on the shadow"
            );
            assert!(v.drift.is_clean(), "no drift on a fresh DB");
        }
        other => panic!("expected Validate report, got {other:?}"),
    }

    // The REAL DB saw no DDL: the schema/table were created only on the shadow.
    assert!(
        !table_exists(&conn, &schema, "only_on_shadow").await,
        "validate must NOT create the table on the real DB"
    );
    let schema_rows = conn
        .query(
            "SELECT 1 FROM information_schema.schemata WHERE schema_name = $1",
            &[&schema],
        )
        .await
        .expect("query schema existence");
    assert!(
        schema_rows.is_empty(),
        "validate must NOT create the schema on the real DB"
    );

    drop_schemas(&conn, &schema, &meta).await;
}
