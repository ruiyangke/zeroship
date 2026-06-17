//! Faithful baseline (Plan 9 sub-feature A) tests against a REAL Postgres.
//!
//! Baseline is the ADOPTION path: an existing project DB already carries its
//! schema; `baseline` records a baseline migration as `completed` WITHOUT running
//! its `up`. These tests prove: the `up` is recorded-not-run (it would FAIL if
//! run, since the table already exists); a subsequent normal apply runs on top;
//! baselining a DB the engine already manages is refused; and a baseline carrying
//! a denied/cross-schema construct is guard-denied.

use std::time::Duration;

use compio_postgres::Client;
use zeroship_migrate::baseline::BaselineError;
use zeroship_migrate::migration::Checksum;
use zeroship_migrate::{
    apply, baseline, ensure_journal, journal, Approval, ExecutorConfig, Migration, MigrationFlags,
    MigrationId,
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
    c.meta_schema = format!("meta_{tok}");
    c.statement_timeout = Duration::from_secs(30);
    c.lock_timeout = Duration::from_secs(10);
    c
}

async fn ensure_project_schema(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS \"{}\"",
        cfg.project_schema
    ))
    .await
    .expect("create project schema");
}

async fn drop_schemas(conn: &Client, cfg: &ExecutorConfig) {
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.meta_schema
        ))
        .await;
}

fn mig(version: MigrationId, name: &str, up: &str) -> Migration {
    Migration {
        version,
        name: name.to_string(),
        up: up.to_string(),
        down: None,
        checksum: Checksum::of(&zeroship_migrate::ChecksumInput {
            up,
            down: None,
            flags: &MigrationFlags::default(),
            owner_app: "app_test",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        }),
        flags: MigrationFlags::default(),
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
    }
}

async fn table_exists(conn: &Client, schema: &str, table: &str) -> bool {
    conn.query_one(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_schema = $1 AND table_name = $2) AS e",
        &[&schema, &table],
    )
    .await
    .expect("table_exists")
    .get::<_, bool>("e")
}

/// Sub-feature A test 1: baseline an existing DB — the `up` is recorded NOT run.
/// The pre-existing table is created DIRECTLY (not via the engine); the baseline's
/// `up` re-creates it WITHOUT `IF NOT EXISTS`, so if `baseline` ran the up it would
/// error ("relation already exists"). We assert it did NOT error, the journal
/// records it `completed` with `kind='baseline'`, and the table is intact.
#[compio::test]
async fn baseline_records_completed_without_running_up() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    // Pre-existing schema, created OUTSIDE the engine.
    conn.batch_execute(&format!(
        "CREATE TABLE \"{}\".\"users\" (id bigint PRIMARY KEY, email text)",
        cfg.project_schema
    ))
    .await
    .expect("seed pre-existing table");

    // The baseline `up` re-creates the SAME table with NO `IF NOT EXISTS`: running
    // it on the existing DB would fail. baseline must NOT run it.
    let up = format!(
        "CREATE TABLE \"{}\".\"users\" (id bigint PRIMARY KEY, email text)",
        cfg.project_schema
    );
    let v = MigrationId::generate();
    let m = mig(v.clone(), "baseline_v0", &up);

    let out = baseline(&conn, &cfg, &m, "operator")
        .await
        .expect("baseline must succeed (the up is recorded, not run)");
    assert_eq!(out.version, v.as_str());
    assert!(!out.already_present);

    // The pre-existing table is intact (not dropped/recreated).
    assert!(table_exists(&conn, &cfg.project_schema, "users").await);

    // The journal records it net-applied as a baseline.
    let applied = journal::applied(&conn, &cfg).await.expect("applied");
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].version, v.as_str());
    let kind: String = conn
        .query_one(
            &format!(
                "SELECT kind FROM \"{}\".schema_migrations WHERE version = $1",
                cfg.meta_schema
            ),
            &[&v.as_str()],
        )
        .await
        .expect("kind")
        .get("kind");
    assert_eq!(kind, "baseline");

    drop_schemas(&conn, &cfg).await;
}

/// Sub-feature A test 2: after baseline, a NEW migration applies on top normally.
#[compio::test]
async fn apply_runs_on_top_of_a_baseline() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    conn.batch_execute(&format!(
        "CREATE TABLE \"{}\".\"users\" (id bigint PRIMARY KEY)",
        cfg.project_schema
    ))
    .await
    .expect("seed");

    let bv = MigrationId::generate();
    let base = mig(
        bv.clone(),
        "baseline_v0",
        &format!(
            "CREATE TABLE \"{}\".\"users\" (id bigint PRIMARY KEY)",
            cfg.project_schema
        ),
    );
    baseline(&conn, &cfg, &base, "operator")
        .await
        .expect("baseline");

    // A NEW migration, version AFTER the baseline, creates another table.
    let nv = MigrationId::generate();
    let next = mig(
        nv.clone(),
        "add_orders",
        &format!("CREATE TABLE \"{}\".\"orders\" (id bigint)", cfg.project_schema),
    );
    // The full set must include the baseline (full-history contract) + the new one.
    let out = apply(&conn, &cfg, &[base.clone(), next.clone()], Approval::None, "ci")
        .await
        .expect("apply on top of baseline");
    // Only the new migration runs; the baseline is skipped (already net-applied).
    assert_eq!(out.applied, vec![nv.as_str().to_string()]);
    assert!(table_exists(&conn, &cfg.project_schema, "orders").await);

    drop_schemas(&conn, &cfg).await;
}

/// Sub-feature A test 3: baselining a DB the engine already manages is refused.
#[compio::test]
async fn baseline_refused_when_journal_has_applied_migration() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("journal");

    // Normal apply first (engine now manages real history).
    let v1 = MigrationId::generate();
    let m1 = mig(
        v1.clone(),
        "create_t",
        &format!("CREATE TABLE \"{}\".\"t\" (id bigint)", cfg.project_schema),
    );
    apply(&conn, &cfg, std::slice::from_ref(&m1), Approval::None, "ci")
        .await
        .expect("apply");

    // Now baselining is refused.
    let bv = MigrationId::generate();
    let base = mig(
        bv,
        "baseline_v0",
        &format!("CREATE TABLE \"{}\".\"u\" (id bigint)", cfg.project_schema),
    );
    let err = baseline(&conn, &cfg, &base, "operator")
        .await
        .expect_err("baseline must be refused once the engine manages history");
    assert!(
        matches!(err, BaselineError::AlreadyManaged { existing, .. } if existing == 1),
        "got {err:?}"
    );

    drop_schemas(&conn, &cfg).await;
}

/// Sub-feature A test 4: a baseline whose `up` contains a cross-schema/denied
/// construct is guard-denied (it represents real schema and is held to the same
/// deny-list as any up, even though it never runs).
#[compio::test]
async fn baseline_guard_denies_cross_schema_up() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    // The `up` references the `control` schema — cross-tenant, must be denied.
    let bv = MigrationId::generate();
    let base = mig(
        bv.clone(),
        "evil_baseline",
        "CREATE TABLE control.users (id bigint)",
    );
    let err = baseline(&conn, &cfg, &base, "operator")
        .await
        .expect_err("cross-schema baseline must be guard-denied");
    assert!(
        matches!(err, BaselineError::Guard { ref version, .. } if version == bv.as_str()),
        "got {err:?}"
    );
    // Nothing journaled.
    let applied = journal::applied(&conn, &cfg).await.unwrap_or_default();
    assert!(applied.is_empty());

    drop_schemas(&conn, &cfg).await;
}

/// Sub-feature A test 5: re-baselining the SAME version is idempotent.
#[compio::test]
async fn re_baseline_same_version_is_idempotent() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    conn.batch_execute(&format!(
        "CREATE TABLE \"{}\".\"t\" (id bigint)",
        cfg.project_schema
    ))
    .await
    .expect("seed");

    let bv = MigrationId::generate();
    let base = mig(
        bv.clone(),
        "baseline_v0",
        &format!("CREATE TABLE \"{}\".\"t\" (id bigint)", cfg.project_schema),
    );
    let out1 = baseline(&conn, &cfg, &base, "operator").await.expect("1st");
    assert!(!out1.already_present);
    let out2 = baseline(&conn, &cfg, &base, "operator").await.expect("2nd");
    assert!(out2.already_present, "re-baseline of same version is a no-op");

    // Exactly ONE baseline event recorded.
    let n: i64 = conn
        .query_one(
            &format!(
                "SELECT count(*)::bigint AS n FROM \"{}\".schema_migrations",
                cfg.meta_schema
            ),
            &[],
        )
        .await
        .expect("count")
        .get("n");
    assert_eq!(n, 1);

    drop_schemas(&conn, &cfg).await;
}
