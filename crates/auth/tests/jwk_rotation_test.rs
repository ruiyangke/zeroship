//! JWK rotation cron — live PG + live hydra. Skip when env unset.
//!
//! Drives [`jwk_rotation::tick_once_for_test`] directly so we exercise
//! the full PG↔hydra interaction without sitting on a real 24 h sleep.
//!
//! These tests mutate the live hydra's JWKS for the test database — they
//! must run against the integration hydra (the smoke fixture's hydra),
//! NOT a production instance. CI gates them on `AUTH_DB_URL`+`AUTH_HYDRA_ADMIN`
//! being set; absent either, the tests print `skip` and pass.

use compio_postgres::{connect, NoTls};
use zeroship_auth::cron::jwk_rotation;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::store::migrations;

#[compio::test]
async fn rotation_first_tick_records_baseline_no_action() {
    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skip: AUTH_DB_URL unset");
        return;
    };
    let Ok(admin_url) = std::env::var("AUTH_HYDRA_ADMIN") else {
        eprintln!("skip: AUTH_HYDRA_ADMIN unset");
        return;
    };

    let (client, conn) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    migrations::migrate(&client).await.expect("migrate");

    // Ensure no prior cron_state for these sets so we exercise the
    // first-observation branch.
    client
        .execute(
            "DELETE FROM auth.cron_state WHERE key LIKE 'hydra.%'",
            &[],
        )
        .await
        .ok();

    let admin = HydraAdmin::new(&admin_url);

    // Capture current JWKS count for the ID-token set. Bootstrap must
    // already have populated it (the integration hydra is bootstrapped
    // by the broader test fixture, not by us here).
    let before = admin
        .get_jwks("hydra.openid.id-token")
        .await
        .expect("get_jwks")
        .expect("id-token set populated by bootstrap");

    // First tick: should INITIALISE baseline + take no action (no
    // rotation due — we just observed the set).
    jwk_rotation::tick_once_for_test(&admin, &client, 90, 31)
        .await
        .expect("tick");

    // Verify cron_state row exists for the id-token set.
    let row = client
        .query_one(
            "SELECT key FROM auth.cron_state WHERE key = $1",
            &[&"hydra.openid.id-token"],
        )
        .await
        .expect("query");
    let key: String = row.get("key");
    assert_eq!(key, "hydra.openid.id-token");

    // Verify JWKS count unchanged (no rotation on first tick).
    let after = admin
        .get_jwks("hydra.openid.id-token")
        .await
        .expect("get_jwks")
        .expect("id-token set still present");
    assert_eq!(
        before.keys.len(),
        after.keys.len(),
        "first tick must NOT rotate; only plant baseline"
    );

    // Cleanup so re-runs in the same DB are deterministic.
    client
        .execute(
            "DELETE FROM auth.cron_state WHERE key LIKE 'hydra.%'",
            &[],
        )
        .await
        .ok();
}

#[compio::test]
async fn rotation_due_prepends_new_keys() {
    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skip: AUTH_DB_URL unset");
        return;
    };
    let Ok(admin_url) = std::env::var("AUTH_HYDRA_ADMIN") else {
        eprintln!("skip: AUTH_HYDRA_ADMIN unset");
        return;
    };

    let (client, conn) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    migrations::migrate(&client).await.expect("migrate");

    let admin = HydraAdmin::new(&admin_url);

    // Seed `last_rotated_at` 100 days in the past for the id-token set
    // so this tick treats it as overdue.
    client
        .execute(
            "INSERT INTO auth.cron_state (key, last_rotated_at) \
             VALUES ($1, NOW() - INTERVAL '100 days') \
             ON CONFLICT (key) DO UPDATE SET last_rotated_at = NOW() - INTERVAL '100 days'",
            &[&"hydra.openid.id-token"],
        )
        .await
        .expect("seed");
    // Also seed access-token set so its retire branch doesn't bail —
    // but well within the rotation window, so we're only asserting the
    // id-token rotation behaviour.
    client
        .execute(
            "INSERT INTO auth.cron_state (key, last_rotated_at) VALUES ($1, NOW()) \
             ON CONFLICT (key) DO UPDATE SET last_rotated_at = NOW()",
            &[&"hydra.jwt.access-token"],
        )
        .await
        .expect("seed access");

    let before = admin
        .get_jwks("hydra.openid.id-token")
        .await
        .expect("get_jwks")
        .expect("id-token set populated by bootstrap");

    jwk_rotation::tick_once_for_test(&admin, &client, 90, 31)
        .await
        .expect("tick");

    let after = admin
        .get_jwks("hydra.openid.id-token")
        .await
        .expect("get_jwks")
        .expect("id-token set still present");
    assert!(
        after.keys.len() > before.keys.len(),
        "expected at least one new key prepended (algs: EdDSA + RS256 → +2). \
         before={}, after={}",
        before.keys.len(),
        after.keys.len(),
    );

    // Cleanup: the rotation state stays such that follow-up runs in
    // the same DB don't re-rotate. We deliberately leave the freshly
    // prepended keys in hydra — retiring them risks taking the
    // integration hydra below the bootstrap minimum mid-test. The
    // retire-stale-keys path is exercised by the unit-level
    // threshold tests; the live path's behaviour is the same code.
    client
        .execute(
            "DELETE FROM auth.cron_state WHERE key LIKE 'hydra.%'",
            &[],
        )
        .await
        .ok();
}
