//! Audit-retention cron — live PG. Skip when `AUTH_DB_URL` unset.
//!
//! Drives [`audit_retention::tick`] directly so the sweep is observable
//! inside a single test run (the real cron sleeps 1 h between ticks).
//! Each test tags its rows with a unique `detail.tag` UUID so concurrent
//! runs against the same DB don't trip over each other.

use compio_postgres::{connect, NoTls};
use uuid::Uuid;
use zeroship_auth::cron::audit_retention;

#[compio::test]
async fn retention_deletes_old_security_events() {
    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skip: AUTH_DB_URL unset");
        return;
    };
    let (client, conn) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();

    // Use a tag in the detail json to uniquely identify our test rows.
    let test_tag = format!("retention-test-{}", Uuid::new_v4().simple());

    // Seed: an old security event (366 days ago) + a young one (1 day ago).
    client
        .execute(
            "INSERT INTO zeroship.audit_events (event_type, outcome, occurred_at, detail) \
             VALUES ($1, 'success', NOW() - INTERVAL '366 days', $2::jsonb)",
            &[&"login_success", &serde_json::json!({ "tag": test_tag.clone() })],
        )
        .await
        .expect("seed old");

    client
        .execute(
            "INSERT INTO zeroship.audit_events (event_type, outcome, occurred_at, detail) \
             VALUES ($1, 'success', NOW() - INTERVAL '1 day', $2::jsonb)",
            &[&"login_success", &serde_json::json!({ "tag": test_tag.clone() })],
        )
        .await
        .expect("seed young");

    // Run tick.
    audit_retention::tick(&client).await.expect("tick");

    // The old row should be gone; the young one still present.
    let rows = client
        .query(
            "SELECT 1 AS one FROM zeroship.audit_events WHERE detail->>'tag' = $1",
            &[&test_tag],
        )
        .await
        .expect("query");
    assert_eq!(rows.len(), 1, "expected only the young row to survive");

    // Cleanup.
    client
        .execute(
            "DELETE FROM zeroship.audit_events WHERE detail->>'tag' = $1",
            &[&test_tag],
        )
        .await
        .ok();
}

#[compio::test]
async fn retention_keeps_refresh_reuse_detected_forever() {
    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skip: AUTH_DB_URL unset");
        return;
    };
    let (client, conn) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();

    let test_tag = format!("retention-reuse-{}", Uuid::new_v4().simple());

    // Seed an ancient refresh_reuse_detected row. 2000 days is well past
    // every bucket's TTL — if any bucket accidentally included this
    // event_type, the row would be swept.
    client
        .execute(
            "INSERT INTO zeroship.audit_events (event_type, outcome, occurred_at, detail) \
             VALUES ('refresh_reuse_detected', 'failure', NOW() - INTERVAL '2000 days', $1::jsonb)",
            &[&serde_json::json!({ "tag": test_tag.clone() })],
        )
        .await
        .expect("seed");

    audit_retention::tick(&client).await.expect("tick");

    let rows = client
        .query(
            "SELECT 1 AS one FROM zeroship.audit_events WHERE detail->>'tag' = $1",
            &[&test_tag],
        )
        .await
        .expect("query");
    assert_eq!(rows.len(), 1, "refresh_reuse_detected must NEVER be swept");

    // Cleanup.
    client
        .execute(
            "DELETE FROM zeroship.audit_events WHERE detail->>'tag' = $1",
            &[&test_tag],
        )
        .await
        .ok();
}
