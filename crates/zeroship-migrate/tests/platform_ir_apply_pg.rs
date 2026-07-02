//! Live-PG coverage for the committed-IR apply core that creator deploys still use.
//!
//! Model C removed the Platform runner's committed `.ir.json` corpus branch: platform
//! migrations are `.ts`-only and record transient IR at migrate time. The shared
//! committed-IR apply core remains for creator/control-plane deploys, so this file
//! keeps the Confined denial coverage and pins the runner's Platform IR refusal.

use std::path::{Path, PathBuf};

use compio_postgres::Client;
use zeroship_migrate::command::runner::{run_migrate, RunConfig, RunProfile};
use zeroship_migrate::test_support::acquire_global_platform_resource_lock;
use zeroship_migrate::{Approval, ExecutorConfig, GuardConfig, PostgresBackend};

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_ir_test";
const TEST_DB_NAME: &str = "zeroship_migrate_ir_test";

const CONFINED_ROLE: &str = "zeroship_ir_confined_role";

fn dsn() -> String {
    std::env::var("MIGRATE_PLATFORM_IR_TEST_DB").unwrap_or_else(|_| DEFAULT_DSN.to_string())
}

fn maintenance_dsn() -> String {
    dsn()
        .split_whitespace()
        .map(|tok| {
            if tok.starts_with("dbname=") {
                "dbname=postgres".to_string()
            } else {
                tok.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

async fn ensure_dedicated_db() {
    let raw = dsn();
    if !raw.contains(&format!("dbname={TEST_DB_NAME}")) {
        return;
    }
    let (client, conn) = compio_postgres::connect(&maintenance_dsn(), compio_postgres::NoTls)
        .await
        .expect("connect to maintenance postgres DB on :5440");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    let exists = !client
        .query("SELECT 1 FROM pg_database WHERE datname = $1", &[&TEST_DB_NAME])
        .await
        .expect("query pg_database")
        .is_empty();
    if !exists {
        if let Err(e) = client
            .batch_execute(&format!(r#"CREATE DATABASE "{TEST_DB_NAME}""#))
            .await
        {
            let exists_after_race = !client
                .query("SELECT 1 FROM pg_database WHERE datname = $1", &[&TEST_DB_NAME])
                .await
                .expect("query pg_database after create failure")
                .is_empty();
            assert!(
                exists_after_race,
                "create dedicated platform IR test database failed and DB is still absent: {e}"
            );
        }
    }
}

async fn pg() -> Client {
    let (client, conn) = compio_postgres::connect(&dsn(), compio_postgres::NoTls)
        .await
        .expect("connect to zeroship_migrate_ir_test on :5440");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

fn token() -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid}_{nanos}")
}

fn fixture_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/platform_ir_fixtures")
        .join(name)
}

fn platform_cfg(dir: &Path, meta: &str, yes: bool) -> RunConfig {
    RunConfig {
        dir: dir.to_path_buf(),
        database_url: dsn(),
        engine_override: None,
        profile: RunProfile::Platform,
        project_id: "platform-ir-test".to_string(),
        project_schema: "zeroship".to_string(),
        schemas: vec!["zeroship".to_string(), "public".to_string()],
        extensions: vec!["citext".to_string()],
        meta_schema: meta.to_string(),
        yes,
        statement_timeout: std::time::Duration::from_secs(60),
        lock_timeout: std::time::Duration::from_secs(30),
    }
}

fn confined_exec_cfg(meta: &str) -> ExecutorConfig {
    let mut cfg = ExecutorConfig::new("confined-ir-test", "zeroship");
    cfg.pg.meta_schema = meta.to_string();
    cfg
}

async fn reset(conn: &Client, meta: &str) {
    conn.batch_execute(
        r#"DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_ir_confined_role') THEN
    DROP OWNED BY "zeroship_ir_confined_role";
  END IF;
END $$;"#,
    )
    .await
    .expect("drop owned by test roles");
    conn.batch_execute(&format!(
        "DROP SCHEMA IF EXISTS zeroship CASCADE; \
         DROP SCHEMA IF EXISTS \"{meta}\" CASCADE; \
         DROP EXTENSION IF EXISTS citext CASCADE;"
    ))
    .await
    .expect("reset schemas/extensions");
    conn.batch_execute(&format!("DROP ROLE IF EXISTS \"{CONFINED_ROLE}\";"))
        .await
        .expect("drop test roles");
}

async fn role_exists(conn: &Client, role: &str) -> bool {
    !conn
        .query("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&role])
        .await
        .expect("query pg_roles")
        .is_empty()
}

async fn table_exists(conn: &Client, schema: &str, table: &str) -> bool {
    !conn
        .query(
            "SELECT 1 FROM information_schema.tables WHERE table_schema = $1 AND table_name = $2",
            &[&schema, &table],
        )
        .await
        .expect("query information_schema.tables")
        .is_empty()
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tok: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("zsmig_platform_ir_{tok}"));
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

#[compio::test]
async fn platform_runner_rejects_committed_ir_corpus() {
    let cfg = platform_cfg(&fixture_dir("apply"), "platform_ir_meta_unused", false);
    let err = run_migrate(&cfg)
        .await
        .expect_err("Platform runner no longer accepts committed .ir.json corpora");
    assert!(
        format!("{err}").contains("unsupported platform migration corpus"),
        "got: {err}"
    );
}

#[compio::test]
async fn confined_ir_still_denies_role_and_grant_vendor_ops() {
    ensure_dedicated_db().await;
    let global_lock = acquire_global_platform_resource_lock(&dsn()).await;
    let conn = pg().await;
    let backend = PostgresBackend::new(&conn);
    let guard = GuardConfig::confined("zeroship");

    let role_meta = format!("confined_ir_role_meta_{}", token());
    reset(&conn, &role_meta).await;
    let role_cfg = confined_exec_cfg(&role_meta);
    let role_err = zeroship_migrate::apply_bundle_ir_postgres(
        &backend,
        "zeroship",
        "app_confined",
        &fixture_dir("confined_role"),
        &role_cfg,
        &guard,
        Approval::Approved,
        "confined-ir-test",
    )
    .await
    .expect_err("Confined IR must reject createRole");
    let role_msg = format!("{role_err}").to_ascii_lowercase();
    assert!(
        role_msg.contains("role") || role_msg.contains("vendor_op_denied"),
        "role denial should identify the denied role primitive, got: {role_err}"
    );
    assert!(
        !role_exists(&conn, CONFINED_ROLE).await,
        "denied createRole did not materialize"
    );

    let grant_meta = format!("confined_ir_grant_meta_{}", token());
    reset(&conn, &grant_meta).await;
    let grant_cfg = confined_exec_cfg(&grant_meta);
    let grant_err = zeroship_migrate::apply_bundle_ir_postgres(
        &backend,
        "zeroship",
        "app_confined",
        &fixture_dir("confined_grant"),
        &grant_cfg,
        &guard,
        Approval::Approved,
        "confined-ir-test",
    )
    .await
    .expect_err("Confined IR must reject grant");
    let grant_msg = format!("{grant_err}").to_ascii_lowercase();
    assert!(
        grant_msg.contains("grant") || grant_msg.contains("vendor_op_denied"),
        "grant denial should identify grant, got: {grant_err}"
    );
    assert!(
        !table_exists(&conn, "zeroship", "ir_confined_grants").await,
        "guard-per-fragment denial happens before the createTable applies"
    );

    reset(&conn, &grant_meta).await;
    global_lock.release().await;
}

#[compio::test]
async fn platform_runner_rejects_mixed_ts_ir_and_sql_corpus_before_connect() {
    let tok = token();
    let dir = TempDir::new(&tok);
    dir.write("20260630000000_mixed.ts", "export function up() {}\n");
    dir.write("20260630000000_mixed.ir.json", "{}");
    dir.write("V0001__mixed.sql", "SELECT 1;");

    let cfg = platform_cfg(dir.path(), "mixed_meta", false);
    let err = run_migrate(&cfg)
        .await
        .expect_err("mixed TS/IR/SQL corpus must be rejected before loader/apply");
    assert!(
        format!("{err}").contains("mixed platform migration corpus"),
        "got: {err}"
    );
}
