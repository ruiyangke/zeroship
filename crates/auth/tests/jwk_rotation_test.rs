//! JWK rotation cron — live PG + live hydra. Skip when env unset.
//!
//! Drives [`jwk_rotation::tick_once_for_test`] directly so we exercise
//! the full PG↔hydra interaction without sitting on a real 24 h sleep.
//!
//! These tests mutate the live hydra's JWKS for the test database — they
//! must run against the integration hydra (the smoke fixture's hydra),
//! NOT a production instance. CI gates them on `AUTH_DB_URL`+`AUTH_HYDRA_ADMIN`
//! being set; absent either, the tests print `skip` and pass.

// Holding a sync mutex across awaits is the entire point of
// `JWK_TEST_LOCK` — see comment on the static below. Each
// `#[compio::test]` spins up its own single-threaded compio runtime,
// so this can't deadlock: the contender for the mutex is in a
// SEPARATE thread / runtime.
#![allow(clippy::await_holding_lock)]

use compio_postgres::{connect, Client, NoTls};
use std::sync::Mutex;
use zeroship_auth::cron::jwk_rotation;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::store::migrations;

// The hydra-backed tests in this file mutate the same `auth.cron_state` rows
// (`hydra.openid.id-token`, `hydra.jwt.access-token`) and the same
// live hydra JWKS sets. When cargo's test runner schedules them in
// parallel, one test's `DELETE FROM auth.cron_state` clobbers the
// other's setup — flaky.
//
// We serialize the two via a file-local mutex. `#[serial_test::serial]`
// would be cleaner but doesn't compose with `#[compio::test]` (the
// compio attribute consumes the inner `async fn` and emits a sync
// `#[test]` wrapper, leaving no obvious place for the `serial_test`
// macro to splice in). A plain `Mutex<()>` guard at the top of each
// test body achieves the same effect without coupling to macro order.
//
// Other test files are unaffected — they target different DB rows
// / hydra sets and remain parallel-safe with this file.
static JWK_TEST_LOCK: Mutex<()> = Mutex::new(());

async fn pg_connect(dsn: &str) -> Client {
    let (client, conn) = connect(dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

async fn clear_jwk_state(client: &Client) {
    client
        .execute(
            "DELETE FROM auth.jwk_key_state WHERE set_name LIKE 'hydra.%'",
            &[],
        )
        .await
        .ok();
    client
        .execute(
            "DELETE FROM auth.cron_state WHERE key LIKE 'hydra.%'",
            &[],
        )
        .await
        .ok();
}

#[compio::test]
async fn rotation_first_tick_records_baseline_no_action() {
    let _guard = JWK_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skip: AUTH_DB_URL unset");
        return;
    };
    let Ok(admin_url) = std::env::var("AUTH_HYDRA_ADMIN") else {
        eprintln!("skip: AUTH_HYDRA_ADMIN unset");
        return;
    };

    let client = pg_connect(&dsn).await;
    migrations::migrate(&client).await.expect("migrate");

    // Ensure no prior cron_state for these sets so we exercise the
    // first-observation branch.
    clear_jwk_state(&client).await;

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
    clear_jwk_state(&client).await;
}

#[compio::test]
async fn rotation_due_prepends_new_keys() {
    let _guard = JWK_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skip: AUTH_DB_URL unset");
        return;
    };
    let Ok(admin_url) = std::env::var("AUTH_HYDRA_ADMIN") else {
        eprintln!("skip: AUTH_HYDRA_ADMIN unset");
        return;
    };

    let client = pg_connect(&dsn).await;
    migrations::migrate(&client).await.expect("migrate");
    clear_jwk_state(&client).await;

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
    clear_jwk_state(&client).await;
}

#[compio::test]
async fn concurrent_rotation_ticks_create_one_key_batch() {
    let _guard = JWK_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skip: AUTH_DB_URL unset");
        return;
    };
    let Ok(admin_url) = std::env::var("AUTH_HYDRA_ADMIN") else {
        eprintln!("skip: AUTH_HYDRA_ADMIN unset");
        return;
    };

    let client = pg_connect(&dsn).await;
    migrations::migrate(&client).await.expect("migrate");
    clear_jwk_state(&client).await;

    let id_token_set = "hydra.openid.id-token";
    let access_set = "hydra.jwt.access-token";
    let rotation_days = 90;
    let retain_days = 31;
    let admin = HydraAdmin::new(&admin_url);

    client
        .execute(
            "INSERT INTO auth.cron_state (key, last_rotated_at) \
             VALUES ($1, NOW() - INTERVAL '100 days') \
             ON CONFLICT (key) DO UPDATE SET last_rotated_at = NOW() - INTERVAL '100 days'",
            &[&id_token_set],
        )
        .await
        .expect("seed due id-token set");
    client
        .execute(
            "INSERT INTO auth.cron_state (key, last_rotated_at) VALUES ($1, NOW()) \
             ON CONFLICT (key) DO UPDATE SET last_rotated_at = NOW()",
            &[&access_set],
        )
        .await
        .expect("seed access-token set");

    let before = admin
        .get_jwks(id_token_set)
        .await
        .expect("get_jwks before")
        .expect("id-token set populated by bootstrap");

    let client_a = pg_connect(&dsn).await;
    let client_b = pg_connect(&dsn).await;
    let admin_a = HydraAdmin::new(&admin_url);
    let admin_b = HydraAdmin::new(&admin_url);
    let tick_a = compio::runtime::spawn(async move {
        jwk_rotation::tick_once_for_test(&admin_a, &client_a, rotation_days, retain_days).await
    });
    let tick_b = compio::runtime::spawn(async move {
        jwk_rotation::tick_once_for_test(&admin_b, &client_b, rotation_days, retain_days).await
    });

    tick_a.await.expect("join tick A").expect("tick A");
    tick_b.await.expect("join tick B").expect("tick B");

    let after = admin
        .get_jwks(id_token_set)
        .await
        .expect("get_jwks after")
        .expect("id-token set still present");
    assert_eq!(
        before.keys.len() + 2,
        after.keys.len(),
        "two concurrent ticks must serialize: one id-token batch only"
    );

    clear_jwk_state(&client).await;
}

#[compio::test]
async fn stale_access_token_keys_are_retired_before_rotation() {
    let _guard = JWK_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skip: AUTH_DB_URL unset");
        return;
    };
    let Ok(admin_url) = std::env::var("AUTH_HYDRA_ADMIN") else {
        eprintln!("skip: AUTH_HYDRA_ADMIN unset");
        return;
    };

    let client = pg_connect(&dsn).await;
    migrations::migrate(&client).await.expect("migrate");
    clear_jwk_state(&client).await;

    let admin = HydraAdmin::new(&admin_url);
    let access_set = "hydra.jwt.access-token";
    let id_token_set = "hydra.openid.id-token";
    let rotation_days = 90;
    let retain_days = 31;
    let retain_count = 1;
    let seed_count = retain_count + 3;

    // Keep the id-token set out of the assertion path. This test is
    // about the access-token set, whose single signing alg makes the
    // expected post-tick key count exact: retain one stale key, then
    // prepend one rotated key.
    client
        .execute(
            "INSERT INTO auth.cron_state (key, last_rotated_at) VALUES ($1, NOW()) \
             ON CONFLICT (key) DO UPDATE SET last_rotated_at = NOW()",
            &[&id_token_set],
        )
        .await
        .expect("seed id-token cron_state");

    for _ in 0..seed_count {
        admin
            .create_jwk(access_set, "EdDSA")
            .await
            .expect("seed access-token jwk");
    }

    let before = admin
        .get_jwks(access_set)
        .await
        .expect("get_jwks before")
        .expect("access-token set exists after seeding");
    assert!(
        before.keys.len() > retain_count,
        "test requires more than retain_count keys before tick; before={}, retain_count={}",
        before.keys.len(),
        retain_count
    );

    client
        .execute(
            "INSERT INTO auth.cron_state (key, last_rotated_at) \
             VALUES ($1, NOW() - (($2::INT + $3::INT + 1) * INTERVAL '1 day')) \
             ON CONFLICT (key) DO UPDATE \
             SET last_rotated_at = NOW() - (($2::INT + $3::INT + 1) * INTERVAL '1 day')",
            &[&access_set, &rotation_days, &retain_days],
        )
        .await
        .expect("seed stale access-token cron_state");

    jwk_rotation::tick_once_for_test(&admin, &client, rotation_days.into(), retain_days.into())
        .await
        .expect("tick");

    let after = admin
        .get_jwks(access_set)
        .await
        .expect("get_jwks after")
        .expect("access-token set still present");
    assert!(
        after.keys.len() <= retain_count + 1,
        "stale keys must be retired before rotation resets cron_state; before={}, after={}, limit={}",
        before.keys.len(),
        after.keys.len(),
        retain_count + 1
    );

    client
        .execute(
            "DELETE FROM auth.cron_state WHERE key LIKE 'hydra.%'",
            &[],
        )
        .await
        .ok();
}
