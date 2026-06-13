//! Integration tests for the spend engine against a live Postgres.
//!
//! Exercises the REAL `SpendEngine` path (price current-period usage via the
//! PR4 catalog → derive SpendState with hysteresis → persist transitions to
//! `zeroship.app_spend_state` + `zeroship.spend_state_history`), not the pure
//! `derive_state` (that is unit-tested in `src/spend.rs`).
//!
//! Set `CONTROL_TEST_DB` to run; tests silently skip otherwise. The DB must
//! have changeset 0039 applied (drop+recreate `zeroship_billing_test`, re-run
//! `ops/db-migrate.sh update`).

use std::collections::HashMap;

use compio_postgres::{connect, NoTls};
use uuid::Uuid;

use zeroship_control::metering::{current_period_start_unix, Metering};
use zeroship_control::spend::SpendEngine;
use zeroship_control::Registry;
use zeroship_core::types::{AppUsage, SpendState, UsageReport};

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

async fn pg(db_url: &str) -> compio_postgres::Client {
    let (client, conn) = connect(db_url, NoTls).await.expect("pg connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

/// Seed a dedicated plan that charges 1 cent per `requests` unit (no included
/// quota, no base fee) with the given `spend_limit_default_cents`, then an app
/// on it. Returns `(plan_id, app_id)`.
async fn make_app_on_priced_plan(
    client: &compio_postgres::Client,
    spend_limit_default_cents: i64,
) -> (String, Uuid) {
    let plan_id = format!("pln_spend_{}", Uuid::new_v4().simple());
    client
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, price_model_json, included_quota_json, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'spend-test', 0, \
                     '{\"requests\":{\"Flat\":{\"rate_cents\":1,\"per_units\":1}}}', '{}', \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', $2)",
            &[&plan_id, &spend_limit_default_cents],
        )
        .await
        .expect("seed priced plan");
    let name = format!("spend-test-{}", Uuid::new_v4());
    let rows = client
        .query(
            "INSERT INTO zeroship.apps (name, plan_id, api_key, api_key_hash) \
             VALUES ($1, $2, $3, '') RETURNING id",
            &[&name, &plan_id, &Uuid::new_v4().to_string()],
        )
        .await
        .expect("insert app");
    (plan_id, rows[0].get("id"))
}

fn report(worker: &str, seq: u64, app: Uuid, requests: u64) -> UsageReport {
    let mut counters = HashMap::new();
    counters.insert(app, AppUsage { requests, ..Default::default() });
    UsageReport {
        worker_id: worker.to_string(),
        report_id: Uuid::now_v7(),
        sequence: seq,
        counters,
    }
}

/// Read the persisted `(state, history_count)` for an app.
async fn read_state(
    client: &compio_postgres::Client,
    app: &Uuid,
) -> (Option<String>, i64) {
    let s = client
        .query(
            "SELECT state FROM zeroship.app_spend_state WHERE app_id = $1",
            &[app],
        )
        .await
        .unwrap();
    let state = s.first().map(|r| r.get::<_, String>("state"));
    let h = client
        .query(
            "SELECT COUNT(*) AS n FROM zeroship.spend_state_history WHERE app_id = $1",
            &[app],
        )
        .await
        .unwrap();
    (state, h[0].get::<_, i64>("n"))
}

#[compio::test]
async fn evaluate_all_persists_and_returns_transitions() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let metering = Metering::new(registry.clone());
    let engine = SpendEngine::new(registry);

    // Plan limit = 100 cents; 1 cent per request. Ingest 100 requests into the
    // CURRENT period ⇒ spend = 100 cents = 100% ⇒ Block.
    let (_plan, app) = make_app_on_priced_plan(&client, 100).await;
    let worker = format!("w-{}", Uuid::new_v4());
    metering
        .ingest(&report(&worker, 1, app, 100))
        .await
        .expect("ingest usage");
    // Sanity: usage landed in the current period.
    let period = current_period_start_unix();
    assert_eq!(metering.total(&app, period, "requests").await.unwrap(), 100);

    // First evaluation: Allow → Block (a transition). Only OUR app should be
    // asserted (the fleet may contain other test apps; filter to ours).
    let transitions = engine.evaluate_all().await.expect("evaluate_all");
    let ours: Vec<_> = transitions.iter().filter(|t| t.app_id == app).collect();
    assert_eq!(ours.len(), 1, "our app transitioned exactly once");
    assert_eq!(ours[0].old, SpendState::Allow);
    assert_eq!(ours[0].new, SpendState::Block);

    let (state, hist) = read_state(&client, &app).await;
    assert_eq!(state.as_deref(), Some("block"), "persisted state is block");
    assert_eq!(hist, 1, "exactly one history row appended on the transition");

    // Idempotent: a second evaluation with unchanged usage/limit yields NO new
    // transition for our app and appends NO new history row.
    let transitions2 = engine.evaluate_all().await.expect("evaluate_all #2");
    assert!(
        !transitions2.iter().any(|t| t.app_id == app),
        "no transition on a stable second tick",
    );
    let (_state2, hist2) = read_state(&client, &app).await;
    assert_eq!(hist2, 1, "no extra history row on a stable tick");
}

#[compio::test]
async fn raising_limit_recovers_block_immediately() {
    // Faithful PG exercise of the raised-limit recovery: an app pinned at Block
    // recovers to Allow on the next tick once `set_limit` raises the cap far
    // above the deadband (deadband would otherwise hold Block at a fixed cap).
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let metering = Metering::new(registry.clone());
    let engine = SpendEngine::new(registry);

    let (_plan, app) = make_app_on_priced_plan(&client, 100).await;
    let worker = format!("w-{}", Uuid::new_v4());
    metering.ingest(&report(&worker, 1, app, 100)).await.unwrap();

    // Tick 1: Block.
    engine.evaluate_all().await.unwrap();
    assert_eq!(read_state(&client, &app).await.0.as_deref(), Some("block"));

    // Raise the override to 100_000 cents → 100 cents is now 0.1% → Allow.
    engine.set_limit(&app, Some(100_000)).await.unwrap();

    // Tick 2: immediate recovery to Allow (deadband bypassed by the limit
    // change), and a second history row recording Block → Allow.
    let t = engine.evaluate_all().await.unwrap();
    let ours: Vec<_> = t.iter().filter(|x| x.app_id == app).collect();
    assert_eq!(ours.len(), 1);
    assert_eq!(ours[0].old, SpendState::Block);
    assert_eq!(ours[0].new, SpendState::Allow);

    let (state, hist) = read_state(&client, &app).await;
    assert_eq!(state.as_deref(), Some("allow"));
    assert_eq!(hist, 2, "Block→Allow appends a second history row");
}
