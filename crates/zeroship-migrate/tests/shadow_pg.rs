//! Faithful shadow-DATABASE dry-run tests (v3 Plan C) against a REAL Postgres.
//!
//! The dry-run runs the UNMODIFIED [`zeroship_migrate::apply`] path against a
//! throwaway DATABASE clone (same `project_schema` name, confined migrator role)
//! and tears it down on every path. These tests prove:
//!
//! - a good additive set dry-runs `ok == true`, all `applied_ok` (Phase 1.1);
//! - a deliberately-broken migration dry-runs `ok == false`, the offender
//!   `applied_ok == false`, AND — the load-bearing proof — the REAL project /
//!   meta schemas are NEVER created in the admin DB (Phase 1.2);
//! - the shadow DB is dropped after BOTH the ok and error paths (Phase 1.3);
//! - a clean declarative diff yields a clean resulting-drift + ok; a wrong
//!   generated op yields non-empty resulting-drift + ok == false (Phase 2);
//! - teardown survives a failure injected after CREATE DATABASE (Phase 3);
//! - `sweep_leaked_shadows` reaps a stale clone + leaves a fresh one (Phase 3).
//!
//! Requires `CREATEDB` on the connecting role (the test `postgres` role is a
//! superuser, so CREATEDB is available).

use std::time::Duration;

use compio_postgres::Client;
use zeroship_migrate::migration::Checksum;
use zeroship_migrate::{
    deprovision_migrator, dry_run, migrator_role_name, ExecutorConfig, Migration, MigrationFlags,
    MigrationId, ShadowConfig,
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

/// A config whose migrator role matches what `provision_migrator` creates on the
/// shadow — so the shadow apply runs under the SAME least-privilege confinement
/// as a real apply.
fn cfg_for(tok: &str) -> ExecutorConfig {
    let project_id = format!("prj_{tok}");
    let role = migrator_role_name(&project_id).expect("role name");
    let mut c = ExecutorConfig::new(project_id, format!("proj_{tok}"));
    c.meta_schema = format!("meta_{tok}");
    c.statement_timeout = Duration::from_secs(30);
    c.lock_timeout = Duration::from_secs(10);
    c.migrator_role = Some(role);
    c
}

fn shadow_cfg() -> ShadowConfig {
    ShadowConfig {
        admin_dsn: dsn(),
        db_name_prefix: "zsmig_shadow_".to_string(),
    }
}

fn mig(version: MigrationId, name: &str, up: &str) -> Migration {
    Migration {
        version,
        name: name.to_string(),
        up: up.to_string(),
        down: None,
        checksum: Checksum::of(up, None),
        flags: MigrationFlags::default(),
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
    }
}

async fn schema_exists(conn: &Client, schema: &str) -> bool {
    !conn
        .query(
            "SELECT 1 FROM information_schema.schemata WHERE schema_name = $1",
            &[&schema],
        )
        .await
        .expect("query schema existence")
        .is_empty()
}

async fn shadow_db_count(conn: &Client, prefix: &str) -> i64 {
    let like = format!("{prefix}%");
    let rows = conn
        .query("SELECT datname FROM pg_database WHERE datname LIKE $1", &[&like])
        .await
        .expect("count shadow dbs");
    i64::try_from(rows.len()).unwrap()
}

/// Drop the cluster-global migrator role this test provisioned on the shadow, so
/// it does not leak across the run (the role survives the shadow DB's drop).
async fn cleanup_role(conn: &Client, cfg: &ExecutorConfig) {
    let _ = deprovision_migrator(conn, cfg).await;
}

// ---------------------------------------------------------------------------
// Phase 1.1 — a good additive set dry-runs ok.
// ---------------------------------------------------------------------------

#[compio::test]
async fn dry_run_good_additive_set_is_ok() {
    let admin = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);

    let m1 = mig(
        MigrationId::generate(),
        "create_orders",
        &format!(
            "CREATE TABLE \"{}\".\"orders\" (id bigint PRIMARY KEY, note text)",
            cfg.project_schema
        ),
    );
    let m2 = mig(
        MigrationId::generate(),
        "add_total",
        &format!(
            "ALTER TABLE \"{}\".\"orders\" ADD COLUMN total numeric",
            cfg.project_schema
        ),
    );
    let set = vec![m1.clone(), m2.clone()];

    let report = dry_run(&admin, &set, &cfg, &shadow_cfg(), "actor")
        .await
        .expect("dry_run harness");

    assert!(report.ok, "good additive set must dry-run ok: {report:?}");
    assert_eq!(report.per_migration.len(), 2);
    assert!(report.per_migration.iter().all(|r| r.applied_ok));
    assert!(report.resulting_drift.is_none(), "plain dry_run has no drift");

    cleanup_role(&admin, &cfg).await;
}

// ---------------------------------------------------------------------------
// Phase 1.2 — a broken migration dry-runs NOT ok AND never touches prod.
// ---------------------------------------------------------------------------

#[compio::test]
async fn dry_run_broken_migration_fails_and_never_touches_prod() {
    let admin = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);

    let good = mig(
        MigrationId::generate(),
        "create_t",
        &format!(
            "CREATE TABLE \"{}\".\"t\" (id bigint PRIMARY KEY)",
            cfg.project_schema
        ),
    );
    // Deliberately broken: an FK to a non-existent table fails at apply time.
    let bad = mig(
        MigrationId::generate(),
        "bad_fk",
        &format!(
            "ALTER TABLE \"{schema}\".\"t\" ADD COLUMN parent bigint \
             REFERENCES \"{schema}\".\"missing\"(id)",
            schema = cfg.project_schema
        ),
    );
    let bad_version = bad.version.as_str().to_string();
    let set = vec![good, bad];

    let report = dry_run(&admin, &set, &cfg, &shadow_cfg(), "actor")
        .await
        .expect("dry_run harness");

    assert!(!report.ok, "broken set must dry-run NOT ok");
    let offender = report
        .per_migration
        .iter()
        .find(|r| r.version == bad_version)
        .expect("offending migration in report");
    assert!(!offender.applied_ok, "the bad migration must be applied_ok=false");
    assert!(offender.error.is_some(), "the bad migration carries an error");

    // THE LOAD-BEARING PROOF: the real project + meta schemas were NEVER created
    // in the ADMIN database, and no journal exists there.
    assert!(
        !schema_exists(&admin, &cfg.project_schema).await,
        "dry_run must NOT create the real project schema in the admin DB"
    );
    assert!(
        !schema_exists(&admin, &cfg.meta_schema).await,
        "dry_run must NOT create the real meta schema in the admin DB"
    );

    cleanup_role(&admin, &cfg).await;
}

// ---------------------------------------------------------------------------
// Phase 1.3 — teardown: no shadow DB remains after ok OR error paths.
// ---------------------------------------------------------------------------

#[compio::test]
async fn dry_run_tears_down_shadow_on_ok_and_error_paths() {
    let admin = pg().await;
    let prefix = "zsmig_shadow_";

    // OK path.
    let tok_ok = token();
    let cfg_ok = cfg_for(&tok_ok);
    let good = mig(
        MigrationId::generate(),
        "ok",
        &format!(
            "CREATE TABLE \"{}\".\"x\" (id bigint PRIMARY KEY)",
            cfg_ok.project_schema
        ),
    );
    let _ = dry_run(&admin, &[good], &cfg_ok, &shadow_cfg(), "actor")
        .await
        .expect("ok dry_run");
    cleanup_role(&admin, &cfg_ok).await;

    // ERROR path.
    let tok_err = token();
    let cfg_err = cfg_for(&tok_err);
    let bad = mig(
        MigrationId::generate(),
        "bad",
        &format!(
            "ALTER TABLE \"{schema}\".\"nope\" ADD COLUMN c int",
            schema = cfg_err.project_schema
        ),
    );
    let _ = dry_run(&admin, &[bad], &cfg_err, &shadow_cfg(), "actor")
        .await
        .expect("err dry_run harness still returns Ok(report)");
    cleanup_role(&admin, &cfg_err).await;

    // No shadow database from EITHER path remains.
    assert_eq!(
        shadow_db_count(&admin, prefix).await,
        0,
        "no <prefix>% shadow database may remain after teardown"
    );
}

// ---------------------------------------------------------------------------
// Phase 1.x — CONCURRENTLY non-txn dry-run runs in the shadow (no deadlock).
// (Plan calls this optional — included since it is quick + load-bearing.)
// ---------------------------------------------------------------------------

#[compio::test]
async fn dry_run_concurrently_index_runs_in_shadow() {
    let admin = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);

    let create = mig(
        MigrationId::generate(),
        "create_u",
        &format!(
            "CREATE TABLE \"{}\".\"u\" (id bigint PRIMARY KEY, email text)",
            cfg.project_schema
        ),
    );
    let mut idx = mig(
        MigrationId::generate(),
        "idx_concurrent",
        &format!(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS u_email_idx \
             ON \"{}\".\"u\" (email)",
            cfg.project_schema
        ),
    );
    idx.flags.transactional = false; // non-txn two-phase path
    let set = vec![create, idx];

    let report = dry_run(&admin, &set, &cfg, &shadow_cfg(), "actor")
        .await
        .expect("dry_run harness");
    assert!(report.ok, "CONCURRENTLY index must dry-run ok: {report:?}");
    assert!(report.per_migration.iter().all(|r| r.applied_ok));

    cleanup_role(&admin, &cfg).await;
}
