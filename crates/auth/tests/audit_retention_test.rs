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

    // Run tick. `tick` opens its OWN dedicated connection from the DSN (so
    // the privileged tamper-trigger GUC never shares a socket with other
    // traffic); the seed/assert client here is independent.
    audit_retention::tick(&dsn).await.expect("tick");

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

    audit_retention::tick(&dsn).await.expect("tick");

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

/// P4-C-F1 regression: the sweep's privileged `zeroship.audit_retention`
/// GUC (which disarms the append-only tamper trigger) must be confined to
/// the sweep's own transaction and MUST NOT bleed onto any subsequent query
/// on the same connection.
///
/// The pre-fix code did a BARE session-level `SET zeroship.audit_retention =
/// 'on'` ... DELETEs ... `SET ... = 'off'` on a shared, pipelined connection.
/// That left the trigger disarmed for the whole sweep window for any other
/// query on that socket, and left the connection holding a session-level
/// `'off'` afterward.
///
/// The fix scopes the flag with `SET LOCAL` inside an explicit transaction on
/// a DEDICATED connection: at COMMIT the GUC auto-reverts to its empty default.
///
/// This test drives the REAL sweep ([`audit_retention::sweep_once`]) on a
/// connection it owns, then on that SAME connection:
///   1. asserts `current_setting` reverted to '' (empty) — NOT the leaky
///      session-level `'off'` the bare-SET code would have left, and NOT 'on';
///   2. asserts a non-sweep DELETE against `zeroship.audit_events` on this
///      connection (outside any sweep transaction) is REJECTED by the tamper
///      trigger — i.e. the trigger is ARMED for everything but the sweep.
#[compio::test]
async fn sweep_guc_does_not_leak_past_its_transaction() {
    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skip: AUTH_DB_URL unset");
        return;
    };
    let (mut client, conn) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();

    // Drive the real sweep on THIS connection. After it returns, its
    // SET LOCAL flag must have reverted at COMMIT.
    audit_retention::sweep_once(&mut client)
        .await
        .expect("sweep_once");

    // (1) The privileged GUC must be the empty default — proving it was
    // transaction-local (`SET LOCAL`) and reverted at COMMIT. The pre-fix
    // bare-session `SET ... = 'off'` would leave 'off' here; a leaked flag
    // would leave 'on'. Only the empty string proves true tx-scoping.
    let setting: String = client
        .query_one(
            "SELECT current_setting('zeroship.audit_retention', true) AS v",
            &[],
        )
        .await
        .expect("read GUC")
        .get("v");
    assert_eq!(
        setting, "",
        "audit_retention GUC must auto-revert to empty after the sweep tx \
         (got {setting:?}); a non-empty value means the privileged flag \
         leaked onto this shared connection"
    );

    // (2) Belt-and-suspenders: a non-sweep DELETE on this same connection,
    // outside any sweep transaction, must hit the ARMED tamper trigger and
    // be rejected. If the flag had leaked, this DELETE would silently succeed.
    let test_tag = format!("retention-leak-{}", Uuid::new_v4().simple());
    client
        .execute(
            "INSERT INTO zeroship.audit_events (event_type, outcome, occurred_at, detail) \
             VALUES ('login_success', 'success', NOW(), $1::jsonb)",
            &[&serde_json::json!({ "tag": test_tag.clone() })],
        )
        .await
        .expect("seed");

    let tamper = client
        .execute(
            "DELETE FROM zeroship.audit_events WHERE detail->>'tag' = $1",
            &[&test_tag],
        )
        .await;
    assert!(
        tamper.is_err(),
        "tamper trigger must reject a non-sweep DELETE — it was disarmed, \
         meaning the sweep's audit_retention flag leaked onto this connection"
    );

    // Cleanup the seeded row via the sanctioned path (a fresh sweep-scoped tx).
    {
        let tx = client.transaction().await.expect("cleanup tx");
        tx.batch_execute("SET LOCAL zeroship.audit_retention = 'on'")
            .await
            .expect("enable");
        tx.execute(
            "DELETE FROM zeroship.audit_events WHERE detail->>'tag' = $1",
            &[&test_tag],
        )
        .await
        .expect("cleanup delete");
        tx.commit().await.expect("cleanup commit");
    }
}
