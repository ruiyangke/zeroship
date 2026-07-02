//! Live-PG coverage for Model C Platform `.ts` migrations.
//!
//! The Platform profile records trusted `.ts` migrations to transient IR at
//! migrate time, then reuses the guarded Platform IR apply core. These fixtures
//! are intentionally `.ts`-only: the runner must not write `.ir.json` or `.sql`.

use std::path::{Path, PathBuf};

use compio_postgres::Client;
use tempfile::TempDir;
use zeroship_migrate::command::runner::{run_migrate, RunConfig, RunProfile, RunReport};
use zeroship_migrate::frontend::recorder_child_path;
use zeroship_migrate::test_support::acquire_global_platform_resource_lock;

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
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid}_{nanos}")
}

fn fixture_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/platform_ts_fixtures")
        .join(name)
}

#[derive(Debug)]
struct IsolatedCorpus {
    _temp: TempDir,
    dir: PathBuf,
    project_id: String,
    role: String,
    accounts_table: String,
    first_ok_table: String,
    denied_table: String,
}

fn assert_safe_ident(ident: &str) {
    assert!(
        !ident.is_empty()
            && ident
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "unsafe test identifier: {ident}"
    );
}

fn quoted_ident(ident: &str) -> String {
    assert_safe_ident(ident);
    format!(r#""{ident}""#)
}

fn isolated_fixture_dir(name: &str, tok: &str) -> IsolatedCorpus {
    let role = format!("zeroship_ts_test_app_{tok}");
    let accounts_table = format!("ts_accounts_{tok}");
    let first_ok_table = format!("ts_first_ok_{tok}");
    let denied_table = format!("ts_should_not_exist_{tok}");
    for ident in [&role, &accounts_table, &first_ok_table, &denied_table] {
        assert_safe_ident(ident);
    }

    let temp = tempfile::Builder::new()
        .prefix("zsmig_platform_ts_")
        .tempdir()
        .expect("create isolated platform TS fixture dir");
    let source = fixture_dir(name);
    for entry in std::fs::read_dir(&source).expect("read platform TS fixture dir") {
        let entry = entry.expect("read platform TS fixture entry");
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let file_name = path.file_name().expect("fixture file name");
        let body = std::fs::read_to_string(&path)
            .expect("read platform TS fixture")
            .replace("zeroship_ts_test_app", &role)
            .replace("ts_accounts", &accounts_table)
            .replace("ts_first_ok", &first_ok_table)
            .replace("ts_should_not_exist", &denied_table);
        std::fs::write(temp.path().join(file_name), body).expect("write isolated TS fixture");
    }
    let dir = temp.path().to_path_buf();
    IsolatedCorpus {
        _temp: temp,
        dir,
        project_id: format!("platform-ts-test-{tok}"),
        role,
        accounts_table,
        first_ok_table,
        denied_table,
    }
}

fn assert_child_built() {
    let bin = recorder_child_path();
    assert!(
        bin.exists(),
        "recorder child binary missing at {} - run `cargo build -p zeroship-migrate --bins` first",
        bin.display()
    );
}

fn assert_no_generated_artifacts(dir: &Path) {
    for entry in std::fs::read_dir(dir).expect("read fixture dir") {
        let entry = entry.expect("read fixture entry");
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        assert!(
            !name.ends_with(".ir.json") && !name.ends_with(".sql"),
            "Platform TS migrate must not write generated artifact {name}"
        );
    }
}

fn platform_cfg(dir: &Path, meta: &str, project_id: &str, yes: bool) -> RunConfig {
    RunConfig {
        dir: dir.to_path_buf(),
        database_url: dsn(),
        engine_override: None,
        profile: RunProfile::Platform,
        project_id: project_id.to_string(),
        project_schema: "zeroship".to_string(),
        schemas: vec!["zeroship".to_string(), "public".to_string()],
        extensions: vec!["citext".to_string()],
        meta_schema: meta.to_string(),
        yes,
        statement_timeout: std::time::Duration::from_secs(60),
        lock_timeout: std::time::Duration::from_secs(30),
    }
}

async fn reset(conn: &Client, meta: &str, role: &str) {
    assert_safe_ident(meta);
    assert_safe_ident(role);
    let role_lit = role;
    let role_ident = quoted_ident(role);
    let meta_ident = quoted_ident(meta);
    conn.batch_execute(&format!(
        r#"DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{role_lit}') THEN
    DROP OWNED BY {role_ident};
  END IF;
END $$;"#,
    ))
    .await
    .expect("drop owned by test roles");
    conn.batch_execute(&format!(
        "DROP SCHEMA IF EXISTS zeroship CASCADE; \
         DROP SCHEMA IF EXISTS {meta_ident} CASCADE; \
         DROP EXTENSION IF EXISTS citext CASCADE; \
         DROP ROLE IF EXISTS {role_ident};"
    ))
    .await
    .expect("reset platform TS schemas/extensions/roles");
}

async fn namespace_exists(conn: &Client, name: &str) -> bool {
    !conn
        .query("SELECT 1 FROM pg_namespace WHERE nspname = $1", &[&name])
        .await
        .expect("query pg_namespace")
        .is_empty()
}

async fn role_exists(conn: &Client, role: &str) -> bool {
    !conn
        .query("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&role])
        .await
        .expect("query pg_roles")
        .is_empty()
}

async fn extension_exists(conn: &Client, extension: &str) -> bool {
    !conn
        .query("SELECT 1 FROM pg_extension WHERE extname = $1", &[&extension])
        .await
        .expect("query pg_extension")
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

async fn table_columns(conn: &Client, schema: &str, table: &str) -> Vec<String> {
    conn.query(
        "SELECT column_name FROM information_schema.columns \
          WHERE table_schema = $1 AND table_name = $2 \
          ORDER BY ordinal_position",
        &[&schema, &table],
    )
    .await
    .expect("query information_schema.columns")
    .into_iter()
    .map(|row| row.get::<_, String>("column_name"))
    .collect()
}

async fn rls_flags(conn: &Client, schema: &str, table: &str) -> (bool, bool) {
    let row = conn
        .query_one(
            "SELECT c.relrowsecurity, c.relforcerowsecurity \
               FROM pg_class c \
               JOIN pg_namespace n ON n.oid = c.relnamespace \
              WHERE n.nspname = $1 AND c.relname = $2",
            &[&schema, &table],
        )
        .await
        .expect("query RLS flags");
    (row.get("relrowsecurity"), row.get("relforcerowsecurity"))
}

async fn policy_exists(conn: &Client, schema: &str, table: &str, policy: &str) -> bool {
    !conn
        .query(
            "SELECT 1 FROM pg_policy p \
               JOIN pg_class c ON c.oid = p.polrelid \
               JOIN pg_namespace n ON n.oid = c.relnamespace \
              WHERE n.nspname = $1 AND c.relname = $2 AND p.polname = $3",
            &[&schema, &table, &policy],
        )
        .await
        .expect("query pg_policy")
        .is_empty()
}

async fn grant_exists(conn: &Client, schema: &str, table: &str, grantee: &str) -> bool {
    !conn
        .query(
            "SELECT 1 FROM information_schema.role_table_grants \
              WHERE table_schema = $1 \
                AND table_name = $2 \
                AND grantee = $3 \
                AND privilege_type = 'SELECT'",
            &[&schema, &table, &grantee],
        )
        .await
        .expect("query role_table_grants")
        .is_empty()
}

async fn completed_journal_count(conn: &Client, meta: &str) -> i64 {
    assert_safe_ident(meta);
    conn.query_one(
        &format!(
            "SELECT count(*)::bigint AS n \
               FROM {}.schema_migrations \
              WHERE event_kind = 'applied' AND phase = 'completed'",
            quoted_ident(meta)
        ),
        &[],
    )
    .await
    .expect("query journal count")
    .get("n")
}

async fn assert_project_lock_released(project_id: &str) {
    let conn = pg().await;
    let row = conn
        .query_one(
            "SELECT pg_try_advisory_lock(hashtext($1)::bigint) AS acquired",
            &[&project_id],
        )
        .await
        .expect("probe project advisory lock");
    let acquired: bool = row.get("acquired");
    assert!(
        acquired,
        "project advisory lock for {project_id} was still held after run_migrate returned"
    );
    conn.execute(
        "SELECT pg_advisory_unlock(hashtext($1)::bigint)",
        &[&project_id],
    )
    .await
    .expect("release project advisory lock probe");
}

#[compio::test]
async fn platform_ts_only_materializes_objects_without_writing_artifacts_and_reruns_noop() {
    assert_child_built();
    let global_lock = acquire_global_platform_resource_lock(&dsn()).await;
    let conn = pg().await;
    let tok = token();
    let meta = format!("platform_ts_meta_{tok}");
    let corpus = isolated_fixture_dir("apply", &tok);
    reset(&conn, &meta, &corpus.role).await;

    assert_no_generated_artifacts(&corpus.dir);

    let cfg = platform_cfg(&corpus.dir, &meta, &corpus.project_id, false);
    let report = run_migrate(&cfg)
        .await
        .expect("Platform .ts corpus records to transient IR and applies");
    let first_applied = match report {
        RunReport::Migrate(outcome) => {
            assert!(!outcome.applied.is_empty(), "fresh TS corpus applies migrations");
            assert!(outcome.skipped.is_empty(), "fresh TS corpus has no skips");
            outcome.applied.len()
        }
        other => panic!("expected Migrate report, got {other:?}"),
    };

    assert!(extension_exists(&conn, "citext").await, "citext extension created");
    assert!(namespace_exists(&conn, "zeroship").await, "zeroship schema created");
    assert!(role_exists(&conn, &corpus.role).await, "platform test role created");
    assert!(
        table_exists(&conn, "zeroship", &corpus.accounts_table).await,
        "zeroship.ts_accounts table created"
    );
    let mut columns = table_columns(&conn, "zeroship", &corpus.accounts_table).await;
    columns.sort();
    assert_eq!(
        columns,
        vec!["app_id", "email", "id"],
        "Platform profile must not inject confined system columns"
    );
    let (rls, force_rls) = rls_flags(&conn, "zeroship", &corpus.accounts_table).await;
    assert!(rls, "RLS enabled on zeroship.ts_accounts");
    assert!(force_rls, "RLS forced on zeroship.ts_accounts");
    assert!(
        policy_exists(&conn, "zeroship", &corpus.accounts_table, "tenant_isolation").await,
        "tenant_isolation policy created"
    );
    assert!(
        grant_exists(&conn, "zeroship", &corpus.accounts_table, &corpus.role).await,
        "SELECT grant materialized for platform role"
    );
    assert_no_generated_artifacts(&corpus.dir);

    let journal_count = completed_journal_count(&conn, &meta).await;
    assert_eq!(journal_count, first_applied as i64, "journal records first apply");

    let report2 = run_migrate(&cfg).await.expect("idempotent Platform TS re-run");
    match report2 {
        RunReport::Migrate(outcome) => {
            assert!(outcome.applied.is_empty(), "re-run applies nothing new");
            assert_eq!(
                outcome.skipped.len(),
                first_applied,
                "re-run skips the same stable journal versions"
            );
        }
        other => panic!("expected Migrate report, got {other:?}"),
    }
    assert_eq!(
        completed_journal_count(&conn, &meta).await,
        journal_count,
        "idempotent re-run does not append journal rows"
    );
    assert_no_generated_artifacts(&corpus.dir);

    assert_project_lock_released(&corpus.project_id).await;
    reset(&conn, &meta, &corpus.role).await;
    global_lock.release().await;
}

#[compio::test]
async fn platform_ts_guard_rejects_denied_host_reaching_op_before_apply() {
    assert_child_built();
    let global_lock = acquire_global_platform_resource_lock(&dsn()).await;
    let conn = pg().await;
    let tok = token();
    let meta = format!("platform_ts_denied_meta_{tok}");
    let corpus = isolated_fixture_dir("denied", &tok);
    reset(&conn, &meta, &corpus.role).await;

    let cfg = platform_cfg(&corpus.dir, &meta, &corpus.project_id, true);
    let err = run_migrate(&cfg)
        .await
        .expect_err("Platform TS must reject host-reaching ALTER SYSTEM");
    let msg = format!("{err}").to_ascii_lowercase();
    assert!(
        msg.contains("alter system") || msg.contains("denied"),
        "error should mention the guard denial, got: {err}"
    );
    assert!(
        !namespace_exists(&conn, "zeroship").await,
        "denied migration did not materialize platform schema"
    );
    assert_no_generated_artifacts(&corpus.dir);

    assert_project_lock_released(&corpus.project_id).await;
    reset(&conn, &meta, &corpus.role).await;
    global_lock.release().await;
}

#[compio::test]
async fn platform_ts_mid_corpus_failure_leaves_prior_file_only_and_no_artifacts() {
    assert_child_built();
    let global_lock = acquire_global_platform_resource_lock(&dsn()).await;
    let conn = pg().await;
    let tok = token();
    let meta = format!("platform_ts_failure_meta_{tok}");
    let corpus = isolated_fixture_dir("failure", &tok);
    reset(&conn, &meta, &corpus.role).await;

    let cfg = platform_cfg(&corpus.dir, &meta, &corpus.project_id, true);
    let err = run_migrate(&cfg)
        .await
        .expect_err("second TS migration must be rejected before apply");
    let msg = format!("{err}").to_ascii_lowercase();
    assert!(
        msg.contains("alter system") || msg.contains("denied"),
        "error should mention the second migration guard denial, got: {err}"
    );
    assert!(
        table_exists(&conn, "zeroship", &corpus.first_ok_table).await,
        "the earlier migration remains applied and journaled"
    );
    assert!(
        !table_exists(&conn, "zeroship", &corpus.denied_table).await,
        "the denied migration's earlier createTable fragment was never applied"
    );
    assert!(
        completed_journal_count(&conn, &meta).await > 0,
        "the earlier migration has completed journal rows"
    );
    assert_no_generated_artifacts(&corpus.dir);

    assert_project_lock_released(&corpus.project_id).await;
    reset(&conn, &meta, &corpus.role).await;
    global_lock.release().await;
}
