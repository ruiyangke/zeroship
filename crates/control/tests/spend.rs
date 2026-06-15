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

/// `SpendEngine::evaluate_all` is a FLEET-WIDE sweep (`SELECT id FROM apps` →
/// price + transition every app). In production it is single-flighted by the
/// reconcile cron's `pg_try_advisory_lock`, so two sweeps never run at once. The
/// default multi-threaded test runner would otherwise run two `evaluate_all`
/// tests concurrently — each seeing the OTHER's freshly-seeded Block-bound app
/// and racing to transition it (double history rows / a stolen transition).
/// Serialize the sweep-driving tests with a process-wide lock to mirror the
/// production single-flight (a poisoned lock from a prior panic is recovered —
/// we still want the next test to run).
static SWEEP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

async fn pg(db_url: &str) -> compio_postgres::Client {
    let (client, conn) = connect(db_url, NoTls).await.expect("pg connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

/// Seed a dedicated plan that charges 1 cent per `requests` unit (no included
/// CU, no base fee) with the given `spend_limit_default_cents`, then an app on
/// it. Returns `(plan_id, app_id)`. Under compute-unit pricing the per-request
/// cost is `weight(requests) × fx`: with the seeded global weight of 1 CU /
/// request and `fx = 10^12` pico-cents/CU (= 1 cent/CU), 1 request = 1 cent —
/// identical to the old per-metric `Flat{1,1}` rate.
async fn make_app_on_priced_plan(
    client: &compio_postgres::Client,
    spend_limit_default_cents: i64,
) -> (String, Uuid) {
    // Ensure the `requests` weight is exactly 1 CU / op for this assertion,
    // independent of any future global seed re-tuning.
    client
        .execute(
            "INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units) \
             VALUES ('requests', 1, 1) \
             ON CONFLICT (metric) DO UPDATE SET units_per_op = 1, per_units = 1",
            &[],
        )
        .await
        .expect("upsert requests weight");
    let plan_id = format!("pln_spend_{}", Uuid::new_v4().simple());
    // fx = 10^12 pico-cents/CU = 1 cent/CU.
    let fx_one_cent: i64 = 1_000_000_000_000;
    client
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'spend-test', 0, 0, $3, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', $2)",
            &[&plan_id, &spend_limit_default_cents, &fx_one_cent],
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
    let _sweep = SWEEP_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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

/// #1 (atomic transition): the `app_spend_state` UPSERT and the
/// `spend_state_history` INSERT must commit together. After a transition, BOTH
/// rows exist AND are mutually consistent — the state row's `spend_cents` /
/// `eval_limit_cents` match the latest history row's `spend_cents` /
/// `limit_cents` (they are written from the same values inside ONE
/// transaction). A crash-induced half-write (state with no history, or vice
/// versa) would fail this consistency check.
#[compio::test]
async fn transition_writes_state_and_history_atomically_and_consistent() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let _sweep = SWEEP_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let registry = Registry::new(&url).await.expect("registry");
    let metering = Metering::new(registry.clone());
    let engine = SpendEngine::new(registry);

    // Limit 100 cents, 1 cent/request, 100 requests ⇒ 100% ⇒ Block.
    let (_plan, app) = make_app_on_priced_plan(&client, 100).await;
    let worker = format!("w-{}", Uuid::new_v4());
    metering.ingest(&report(&worker, 1, app, 100)).await.unwrap();

    let transitions = engine.evaluate_all().await.expect("evaluate_all");
    let ours: Vec<_> = transitions.iter().filter(|t| t.app_id == app).collect();
    assert_eq!(ours.len(), 1, "our app transitioned once");

    // The state row exists.
    let state_rows = client
        .query(
            "SELECT state, spend_cents, eval_limit_cents \
             FROM zeroship.app_spend_state WHERE app_id = $1",
            &[&app],
        )
        .await
        .unwrap();
    assert_eq!(state_rows.len(), 1, "exactly one app_spend_state row");
    let state: String = state_rows[0].get("state");
    let state_spend: i64 = state_rows[0].get("spend_cents");
    let state_limit: i64 = state_rows[0].get("eval_limit_cents");
    assert_eq!(state, "block");

    // A matching history row exists, written in the SAME transaction.
    let hist_rows = client
        .query(
            "SELECT from_state, to_state, spend_cents, limit_cents \
             FROM zeroship.spend_state_history WHERE app_id = $1 ORDER BY at DESC",
            &[&app],
        )
        .await
        .unwrap();
    assert_eq!(hist_rows.len(), 1, "exactly one history row on the transition");
    let h_to: String = hist_rows[0].get("to_state");
    let h_spend: i64 = hist_rows[0].get("spend_cents");
    let h_limit: Option<i64> = hist_rows[0].get("limit_cents");
    assert_eq!(h_to, "block", "history records the Block transition");

    // Consistency: both rows carry the SAME priced spend + effective limit,
    // proving they were written together (atomic), not separately/partially.
    assert_eq!(state_spend, 100, "state row records the priced spend");
    assert_eq!(h_spend, state_spend, "history spend matches state spend");
    assert_eq!(h_limit, Some(state_limit), "history limit matches state eval limit");
}

#[compio::test]
async fn overflowing_spend_is_skipped_not_clamped_and_blocked() {
    // MAJOR-1 REGRESSION: an app whose priced `spend_cents` overflows i64 must be
    // SKIPPED with a warn! by the spend sweep — NOT clamped to i64::MAX and
    // Blocked. This mirrors billing_reconcile's overflow posture (it skips such
    // an app too). Pre-fix the sweep did `i64::try_from(spend_cents).unwrap_or(
    // i64::MAX)` — a silent clamp that wrote a Block state row, so enforcement
    // Blocked an app that reconcile would skip (unbilled): the two disagreed.
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let _sweep = SWEEP_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let registry = Registry::new(&url).await.expect("registry");
    let metering = Metering::new(registry.clone());
    let engine = SpendEngine::new(registry);

    // Force the `requests` weight to exactly 1 CU/op for a deterministic CU total.
    client
        .execute(
            "INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units) \
             VALUES ('requests', 1, 1) \
             ON CONFLICT (metric) DO UPDATE SET units_per_op = 1, per_units = 1",
            &[],
        )
        .await
        .expect("upsert requests weight");

    // Plan: fx = 2 cents/CU (= 2 × FX_SCALE pico-cents/CU), no included CU, no
    // base. With usage = i64::MAX requests ⇒ total_units = i64::MAX (fits u64, so
    // NO ComputeUnitOverflow), overage = 2 × i64::MAX cents ≈ u64::MAX — which
    // EXCEEDS i64::MAX, so the cents→i64 conversion in the sweep overflows and
    // the app must be skipped.
    let fx_two_cents: i64 = 2_000_000_000_000; // 2 × 10^12
    let plan_id = format!("pln_ovf_{}", Uuid::new_v4().simple());
    client
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'ovf-test', 0, 0, $2, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', 100)",
            &[&plan_id, &fx_two_cents],
        )
        .await
        .expect("seed overflow plan");
    let name = format!("ovf-test-{}", Uuid::new_v4());
    let app: Uuid = client
        .query(
            "INSERT INTO zeroship.apps (name, plan_id, api_key, api_key_hash) \
             VALUES ($1, $2, $3, '') RETURNING id",
            &[&name, &plan_id, &Uuid::new_v4().to_string()],
        )
        .await
        .expect("insert app")[0]
        .get("id");

    // Usage = i64::MAX requests in the current period.
    let worker = format!("w-{}", Uuid::new_v4());
    metering
        .ingest(&report(&worker, 1, app, i64::MAX as u64))
        .await
        .expect("ingest overflow usage");

    // The sweep must NOT transition (skip) our app, and write NO state row for it.
    let transitions = engine.evaluate_all().await.expect("evaluate_all");
    assert!(
        !transitions.iter().any(|t| t.app_id == app),
        "an overflowing-spend app is SKIPPED (no transition), not clamped-and-Blocked",
    );
    let (state, hist) = read_state(&client, &app).await;
    assert_eq!(state, None, "no app_spend_state row written for the skipped app");
    assert_eq!(hist, 0, "no spend_state_history row for the skipped app");
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
    let _sweep = SWEEP_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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

// ---------------------------------------------------------------------------
// Redesign regression (change 2): `set_limit` UPSERTs the CONFIG table
// `app_spend_limit` ONLY (not the derived `app_spend_state`), and the fleet
// evaluation reflects that override; AND every transition's
// `spend_state_history` row carries a NON-NULL `period` (the column is NOT NULL
// in the redesigned schema).
// ---------------------------------------------------------------------------

#[compio::test]
async fn set_limit_writes_only_app_spend_limit_and_fleet_eval_reflects_it() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let _sweep = SWEEP_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let registry = Registry::new(&url).await.expect("registry");
    let metering = Metering::new(registry.clone());
    let engine = SpendEngine::new(registry);

    // Plan default = 100c. Ingest 100 requests = 100c ⇒ at the plan default this
    // is 100% ⇒ Block.
    let (_plan, app) = make_app_on_priced_plan(&client, 100).await;
    let worker = format!("w-{}", Uuid::new_v4());
    metering.ingest(&report(&worker, 1, app, 100)).await.unwrap();

    // Set a generous override BEFORE the first eval. It must land in the dedicated
    // CONFIG table `app_spend_limit` — NOT in `app_spend_state` (which has no
    // override column anymore).
    engine.set_limit(&app, Some(100_000)).await.unwrap();

    // The override is in app_spend_limit…
    let override_row = client
        .query(
            "SELECT spend_limit_cents FROM zeroship.app_spend_limit WHERE app_id = $1",
            &[&app],
        )
        .await
        .unwrap();
    assert_eq!(override_row.len(), 1, "set_limit wrote an app_spend_limit row");
    assert_eq!(
        override_row[0].get::<_, Option<i64>>("spend_limit_cents"),
        Some(100_000),
        "the override cents landed in app_spend_limit (the config table)",
    );
    // …and set_limit did NOT create an app_spend_state row (no derived write).
    assert_eq!(
        read_state(&client, &app).await.0,
        None,
        "set_limit must NOT write the derived app_spend_state row",
    );

    // Now the fleet eval must reflect the override: 100c against a 100_000c cap is
    // 0.1% ⇒ Allow (NOT Block at the plan default). This proves the double LEFT
    // JOIN reads the override from app_spend_limit.
    engine.evaluate_all().await.unwrap();
    let (state, _hist) = read_state(&client, &app).await;
    assert_eq!(
        state.as_deref(),
        Some("allow"),
        "the fleet eval honored the app_spend_limit override (Allow, not Block at plan default)",
    );
}

#[compio::test]
async fn transition_history_row_binds_non_null_period() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let _sweep = SWEEP_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let registry = Registry::new(&url).await.expect("registry");
    let metering = Metering::new(registry.clone());
    let engine = SpendEngine::new(registry);

    // 100c cap, 100 requests ⇒ Block (a transition that writes history).
    let (_plan, app) = make_app_on_priced_plan(&client, 100).await;
    let worker = format!("w-{}", Uuid::new_v4());
    metering.ingest(&report(&worker, 1, app, 100)).await.unwrap();
    engine.evaluate_all().await.unwrap();

    // `spend_state_history.period` is NOT NULL in the redesigned schema — the
    // INSERT MUST bind it (this is why the 0041 DDL + the code landed together).
    // Reading the bound period back as a DATE proves the bind. (RED if the code
    // omitted the period bind: the INSERT would fail the NOT NULL, so no row.)
    let rows = client
        .query(
            "SELECT period::date AS period FROM zeroship.spend_state_history \
             WHERE app_id = $1 ORDER BY at DESC LIMIT 1",
            &[&app],
        )
        .await
        .expect("read history period");
    assert_eq!(rows.len(), 1, "the transition wrote a history row");
    let period: chrono::NaiveDate = rows[0].get("period");
    use chrono::Datelike;
    assert_eq!(period.day(), 1, "the history period is a first-of-month billing_period DATE");
}
