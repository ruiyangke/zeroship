//! Faithful status + history read-API tests against a REAL Postgres (no shims).
//!
//! Requires `zeroship_migrate_test` on :5440 (see the test runbook). Each test
//! uses its own meta + project schema (unique token) so the shared DB stays
//! clean.

use compio_postgres::Client;
use zeroship_migrate::migration::Checksum;
use zeroship_migrate::{
    apply, history, rollback, status, Approval, ExecutorConfig, HistoryKind, Migration,
    MigrationFlags, MigrationId, RollbackRequest, RollbackTarget,
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

/// A reversible transactional migration (up + a real down) over the project
/// schema, with a correct checksum.
fn mig(version: MigrationId, name: &str, schema: &str, table: &str) -> Migration {
    let up = format!("CREATE TABLE \"{schema}\".\"{table}\" (id int primary key)");
    let down = format!("DROP TABLE \"{schema}\".\"{table}\"");
    Migration {
        version,
        name: name.to_string(),
        up: up.clone(),
        down: Some(down.clone()),
        checksum: Checksum::of(&up, Some(&down)),
        flags: MigrationFlags::default(),
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
    }
}

// ---------------------------------------------------------------------------

#[compio::test]
async fn status_reports_three_applied_zero_pending_and_current() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let m1 = mig(MigrationId::generate(), "t1", &cfg.project_schema, "t1");
    let m2 = mig(MigrationId::generate(), "t2", &cfg.project_schema, "t2");
    let m3 = mig(MigrationId::generate(), "t3", &cfg.project_schema, "t3");
    let set = vec![m1.clone(), m2.clone(), m3.clone()];

    apply(&conn, &cfg, &set, Approval::None, "actor")
        .await
        .expect("apply 3");

    let st = status(&conn, &cfg, &set).await.expect("status");
    assert_eq!(st.applied.len(), 3, "all 3 applied");
    assert!(st.pending.is_empty(), "nothing pending");
    assert!(st.rolled_back.is_empty(), "nothing rolled back");
    assert_eq!(
        st.current_version.as_ref(),
        Some(&m3.version),
        "current = highest applied (v3)"
    );
    // applied is version-ordered.
    let versions: Vec<&str> = st.applied.iter().map(|e| e.version.as_str()).collect();
    assert_eq!(
        versions,
        vec![m1.version.as_str(), m2.version.as_str(), m3.version.as_str()]
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn status_reports_one_pending_in_topo_order_when_a_fourth_is_added() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let m1 = mig(MigrationId::generate(), "t1", &cfg.project_schema, "t1");
    let m2 = mig(MigrationId::generate(), "t2", &cfg.project_schema, "t2");
    let m3 = mig(MigrationId::generate(), "t3", &cfg.project_schema, "t3");
    let applied_set = vec![m1.clone(), m2.clone(), m3.clone()];
    apply(&conn, &cfg, &applied_set, Approval::None, "actor")
        .await
        .expect("apply 3");

    // Add a 4th to the SUPPLIED set; it is now pending.
    let m4 = mig(MigrationId::generate(), "t4", &cfg.project_schema, "t4");
    let full_set = vec![m1, m2, m3, m4.clone()];

    let st = status(&conn, &cfg, &full_set).await.expect("status");
    assert_eq!(st.applied.len(), 3, "3 still applied");
    assert_eq!(st.pending, vec![m4.version.clone()], "only v4 pending");

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn status_after_rollback_shows_rolled_back_and_pending_again_and_history_keeps_both() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let m1 = mig(MigrationId::generate(), "t1", &cfg.project_schema, "t1");
    let m2 = mig(MigrationId::generate(), "t2", &cfg.project_schema, "t2");
    let m3 = mig(MigrationId::generate(), "t3", &cfg.project_schema, "t3");
    let set = vec![m1.clone(), m2.clone(), m3.clone()];
    apply(&conn, &cfg, &set, Approval::None, "actor")
        .await
        .expect("apply 3");

    // Roll back the newest (v3) — Steps(1).
    rollback(
        &conn,
        &cfg,
        &set,
        RollbackRequest::new(RollbackTarget::Steps(1)),
        Approval::Approved,
        "rollbacker",
    )
    .await
    .expect("rollback v3");

    let st = status(&conn, &cfg, &set).await.expect("status after rollback");
    // v3 is net rolled-back, NOT applied, and pending again.
    assert_eq!(st.applied.len(), 2, "2 applied after rollback (v1,v2)");
    assert_eq!(
        st.current_version.as_ref(),
        Some(&m2.version),
        "current drops to v2"
    );
    assert_eq!(
        st.rolled_back.iter().map(|e| e.version.as_str()).collect::<Vec<_>>(),
        vec![m3.version.as_str()],
        "v3 is rolled_back"
    );
    assert_eq!(
        st.pending,
        vec![m3.version.clone()],
        "v3 is pending again (re-appliable)"
    );

    // History shows BOTH the completed AND the rolled_back event for v3, in order.
    let hist = history(&conn, &cfg).await.expect("history");
    let v3 = m3.version.as_str();
    let v3_events: Vec<HistoryKind> = hist
        .iter()
        .filter(|e| e.version == v3)
        .map(|e| e.kind)
        .collect();
    assert_eq!(
        v3_events,
        vec![HistoryKind::Completed, HistoryKind::RolledBack],
        "history shows v3 applied THEN rolled back, in event order"
    );
    // The full log is event_seq-ordered (monotonic, never collapsed to net state).
    let seqs: Vec<i64> = hist.iter().map(|e| e.event_seq).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "history is event_seq-ordered");
    // 3 completed + 1 rolled_back = 4 events.
    assert_eq!(hist.len(), 4, "4 total events in the audit log");

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn status_of_fresh_project_is_empty() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let m1 = mig(MigrationId::generate(), "t1", &cfg.project_schema, "t1");
    let set = vec![m1.clone()];

    // No apply yet → everything pending, nothing applied, no current.
    let st = status(&conn, &cfg, &set).await.expect("status fresh");
    assert!(st.applied.is_empty());
    assert!(st.rolled_back.is_empty());
    assert_eq!(st.current_version, None);
    assert_eq!(st.pending, vec![m1.version.clone()]);

    let hist = history(&conn, &cfg).await.expect("history fresh");
    assert!(hist.is_empty(), "fresh project has empty audit log");

    drop_schemas(&conn, &cfg).await;
}
