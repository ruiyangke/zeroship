//! Regression test (review P12): `zeroship.app_audit` / `authz_decisions` are
//! append-only — an un-flagged `DELETE` is rejected by the tamper trigger — but
//! the sanctioned `control::cron::audit_retention` sweep (which sets
//! `zeroship.audit_retention = 'on'`) deletes rows past the retention window.
//!
//! Would FAIL pre-fix: before this change the tamper trigger blocked EVERY
//! DELETE (so the retention sweep could never remove a row), and the sweep
//! didn't exist. Set `CONTROL_TEST_DB` to a migrated Postgres URL; skipped
//! otherwise so this file doesn't gate CI without a DB.

use compio_postgres::{connect, NoTls};
use uuid::Uuid;
use zeroship_control::cron::audit_retention;
use zeroship_control::Registry;

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

async fn raw_conn(dsn: &str) -> compio_postgres::Client {
    let (client, conn) = connect(dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

#[compio::test]
async fn app_audit_is_append_only_but_retention_sweep_deletes_old() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let registry = Registry::new(&url).await.expect("registry");
    // Unique app so parallel runs don't collide; the count assertion is
    // app-scoped, and the global sweep only touches >retention rows.
    let name = format!("ret-{}", &Uuid::new_v4().simple().to_string()[..12]);
    let app = registry
        .create_app(&name, "free")
        .await
        .expect("create_app")
        .id;
    let conn = raw_conn(&url).await;

    // One >12-month row and one fresh row for this app.
    conn.execute(
        "INSERT INTO zeroship.app_audit (app_id, action, resource, occurred_at) \
         VALUES ($1, 'old', 'r', now() - interval '2 years')",
        &[&app],
    )
    .await
    .expect("seed old");
    conn.execute(
        "INSERT INTO zeroship.app_audit (app_id, action, resource) VALUES ($1, 'new', 'r')",
        &[&app],
    )
    .await
    .expect("seed new");

    // Un-flagged DELETE is rejected by the append-only tamper trigger.
    let blocked = conn
        .execute("DELETE FROM zeroship.app_audit WHERE app_id = $1", &[&app])
        .await;
    assert!(
        blocked.is_err(),
        "un-flagged DELETE on app_audit must be rejected (append-only)"
    );

    // The sanctioned retention sweep (sets the GUC) removes the >12-month row
    // and keeps the fresh one.
    audit_retention::tick(&registry, 12).await.expect("retention tick");

    let remaining: i64 = conn
        .query_one(
            "SELECT count(*) FROM zeroship.app_audit WHERE app_id = $1",
            &[&app],
        )
        .await
        .expect("count")
        .get(0);
    assert_eq!(
        remaining, 1,
        "retention sweep deletes the >12-month row, keeps the fresh one"
    );
}
