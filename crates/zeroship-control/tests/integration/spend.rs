//! Integration tests for the spend engine against a live Postgres.
//!
//! Exercises the REAL `SpendEngine` path (price current-period usage via the
//! plan catalog → derive SpendState with hysteresis → persist transitions to
//! `zeroship.app_spend_state` + `zeroship.spend_state_history`), not the pure
//! `derive_state` (that is unit-tested in `src/spend.rs`).
//!
//! The database comes from `crate::support::require_control_db`, which refuses the run
//! rather than skipping it or substituting one: the whole target shares one
//! database. Provision it with `tests/provision_test_backends.sh`.


use compio_postgres::{connect, NoTls};
use uuid::Uuid;

use zeroship_control::metering::{current_period_start_unix, Metering, UsageAggregate};
use zeroship_control::spend::SpendEngine;
use zeroship_control::Registry;
use zeroship_core::AppId;

fn db_url() -> String {
    crate::support::require_control_db()
}

async fn pg(db_url: &str) -> compio_postgres::Client {
    let (client, conn) = connect(db_url, NoTls).await.expect("pg connect");
    crate::support::live::spawn(async move {
        let _ = conn.run().await;
    });
    client
}

/// Seed a dedicated plan that charges 1 cent per `requests` unit (no included
/// CU, no base fee) with the given `spend_limit_default_cents`, then an app on
/// it. Returns `(plan_id, app_id)`. Under compute-unit pricing the per-request
/// cost is `weight(requests) × fx`: with the seeded global weight of 1 CU /
/// request and `fx = 10^12` pico-cents/CU (= 1 cent/CU), 1 request = 1 cent.
async fn make_app_on_priced_plan(
    client: &compio_postgres::Client,
    spend_limit_default_cents: i64,
) -> (String, AppId) {
    // The metric catalog must exist before the weight FK. Seed it here so a case
    // is self-sufficient regardless of which sibling cases ran first.
    crate::support::seed_metric_catalog(client, "requests").await;
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
    let app = crate::support::seed_app(client, &name, &plan_id).await;
    (plan_id, app)
}

/// Seed THIS app's current-period `requests` total with a per-app upsert. A
/// fleet-wide `replace_period_snapshot` rewrite deletes every other case's
/// current-period rows, so a case touches only its own row.
async fn seed_requests(client: &compio_postgres::Client, app: &AppId, requests: u64) {
    crate::support::seed_metric_catalog(client, "requests").await;
    let period = current_period_start_unix();
    let requests = i64::try_from(requests).expect("test requests fit i64");
    let prior: Option<i64> = client
        .query(
            "SELECT total FROM zeroship.usage_aggregates \
              WHERE app_id = $1 AND period = $2::date AND metric = 'requests'",
            &[&app.as_str(), &crate::support::period_date(period)],
        )
        .await
        .expect("read prior usage")
        .first()
        .map(|r| r.get("total"));
    // Checked rather than silenced: a seed that reports a decrease means a
    // previous run left a HIGHER total for this app in the same period, so the
    // fixture is lying about its starting state.
    if let Some(prior) = prior {
        assert!(
            prior <= requests,
            "seeding must not lower an existing total; fixture state is dirty: {prior} -> {requests}"
        );
    }
    client
        .execute(
            "INSERT INTO zeroship.usage_aggregates AS u (app_id, period, metric, total, updated_at) \
             VALUES ($1, $2::date, 'requests', $3, NOW()) \
             ON CONFLICT (app_id, period, metric) DO UPDATE SET \
               total = EXCLUDED.total, updated_at = NOW()",
            &[&app.as_str(), &crate::support::period_date(period), &requests],
        )
        .await
        .expect("seed usage");
}

/// Read the persisted `(state, history_count)` for an app.
async fn read_state(client: &compio_postgres::Client, app: &AppId) -> (Option<String>, i64) {
    let s = client
        .query(
            "SELECT state FROM zeroship.app_spend_state WHERE app_id = $1",
            &[&app.as_str()],
        )
        .await
        .unwrap();
    let state = s.first().map(|r| r.get::<_, String>("state"));
    let h = client
        .query(
            "SELECT COUNT(*) AS n FROM zeroship.spend_state_history WHERE app_id = $1",
            &[&app.as_str()],
        )
        .await
        .unwrap();
    (state, h[0].get::<_, i64>("n"))
}

/// A period rewrite that LOWERS a stored total must report it.
///
/// `replace_period_snapshot` is an overwrite, not a `+=`: it writes whatever the source
/// scan saw. If the usage stream ages out early-period events, or a fetch stalls
/// mid-read, the recompute rebuilds a SMALLER total and overwrites the correct one -
/// and spend enforcement then reads the smaller number, so apps escape their limits.
/// It fails OPEN, and silently: missing early-month events are indistinguishable from
/// no usage early in the month.
///
/// A total can only grow within a billing month, so a decrease is the one in-band
/// signal that the projection is unsound. This asserts the signal exists; it does NOT
/// assert any enforcement behaviour, because refusing a decrease is an operator policy
/// call (a legitimate dedup fix or bad-event purge also lowers a total).
#[compio::test(crate = "crate::support::live")]
async fn period_rewrite_reports_a_total_that_shrank() {
    let url = db_url();
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let metering = Metering::new(registry);
    let (_plan, app) = make_app_on_priced_plan(&client, 100).await;
    // `replace_period_snapshot` REWRITES every row for its period, so this case
    // reserves a private calendar month: the rewrite can then only touch this
    // case's own row.
    let period = crate::support::next_isolated_period().await;

    let agg = |total: i64| {
        vec![UsageAggregate {
            app_id: app.clone(),
            metric: "requests".to_string(),
            total,
        }]
    };

    // Establish the period at 100.
    let first = metering
        .replace_period_snapshot(period, &agg(100))
        .await
        .expect("seed period");
    assert!(
        first.decreased.is_empty(),
        "writing a period for the first time cannot be a decrease, got {:?}",
        first.decreased
    );

    // The failure this exists to catch: the scan saw less than last time.
    let shrunk = metering
        .replace_period_snapshot(period, &agg(40))
        .await
        .expect("rewrite period lower");
    assert_eq!(shrunk.decreased.len(), 1, "a shrink must be reported");
    let drop = &shrunk.decreased[0];
    assert_eq!(drop.app_id, app);
    assert_eq!(drop.metric, "requests");
    assert_eq!(drop.prior_total, 100);
    assert_eq!(drop.new_total, 40);
    // Reporting only - the write still lands, by design.
    assert_eq!(metering.total(&app, period, "requests").await.unwrap(), 40);

    // POSITIVE CONTROL. The assertions above are satisfied by an implementation that
    // reports EVERY rewrite as a decrease, which would make the signal useless. Growth
    // and an identical rewrite must both stay silent.
    let grew = metering
        .replace_period_snapshot(period, &agg(250))
        .await
        .expect("rewrite period higher");
    assert!(
        grew.decreased.is_empty(),
        "growth is not a decrease, got {:?}",
        grew.decreased
    );
    let same = metering
        .replace_period_snapshot(period, &agg(250))
        .await
        .expect("rewrite period identical");
    assert!(
        same.decreased.is_empty(),
        "an identical rewrite is not a decrease, got {:?}",
        same.decreased
    );
}

#[compio::test(crate = "crate::support::live")]
async fn evaluate_all_persists_and_returns_transitions() {
    let url = db_url();
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let metering = Metering::new(registry.clone());
    let engine = SpendEngine::new(registry);

    // Plan limit = 100 cents; 1 cent per request. Ingest 100 requests into the
    // CURRENT period ⇒ spend = 100 cents = 100% ⇒ Block.
    let (_plan, app) = make_app_on_priced_plan(&client, 100).await;
    seed_requests(&client, &app, 100).await;
    // Sanity: usage landed in the current period.
    let period = current_period_start_unix();
    assert_eq!(metering.total(&app, period, "requests").await.unwrap(), 100);

    // First evaluation: Allow -> Block. The durable per-app state row is asserted
    // (the fleet contains other test apps; the state row is keyed by our app). The
    // transition write is a compare-and-swap, so a concurrent sibling sweep that
    // derived the same transition cannot append a second history row.
    engine.evaluate_all().await.expect("evaluate_all");
    let (state, hist) = read_state(&client, &app).await;
    assert_eq!(state.as_deref(), Some("block"), "persisted state is block");
    assert_eq!(
        hist, 1,
        "exactly one history row appended on the transition"
    );

    // Idempotent: a second evaluation with unchanged usage/limit appends NO new
    // history row.
    engine.evaluate_all().await.expect("evaluate_all #2");
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
#[compio::test(crate = "crate::support::live")]
async fn transition_writes_state_and_history_atomically_and_consistent() {
    let url = db_url();
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let engine = SpendEngine::new(registry);

    // Limit 100 cents, 1 cent/request, 100 requests ⇒ 100% ⇒ Block.
    let (_plan, app) = make_app_on_priced_plan(&client, 100).await;
    seed_requests(&client, &app, 100).await;

    engine.evaluate_all().await.expect("evaluate_all");

    // The state row exists.
    let state_rows = client
        .query(
            "SELECT state, spend_cents, eval_limit_cents \
             FROM zeroship.app_spend_state WHERE app_id = $1",
            &[&app.as_str()],
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
            &[&app.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(
        hist_rows.len(),
        1,
        "exactly one history row on the transition"
    );
    let h_to: String = hist_rows[0].get("to_state");
    let h_spend: i64 = hist_rows[0].get("spend_cents");
    let h_limit: Option<i64> = hist_rows[0].get("limit_cents");
    assert_eq!(h_to, "block", "history records the Block transition");

    // Consistency: both rows carry the SAME priced spend + effective limit,
    // proving they were written together (atomic), not separately/partially.
    assert_eq!(state_spend, 100, "state row records the priced spend");
    assert_eq!(h_spend, state_spend, "history spend matches state spend");
    assert_eq!(
        h_limit,
        Some(state_limit),
        "history limit matches state eval limit"
    );
}

#[compio::test(crate = "crate::support::live")]
async fn overflowing_spend_is_skipped_not_clamped_and_blocked() {
    // MAJOR-1: an app whose priced `spend_cents` overflows i64 must be SKIPPED
    // with a warn! by the spend sweep — NOT clamped to i64::MAX and Blocked. This
    // mirrors billing_reconcile's overflow posture (it skips such an app too), so
    // enforcement and reconcile agree.
    let url = db_url();
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let engine = SpendEngine::new(registry);

    // The metric catalog must exist before the weight FK.
    crate::support::seed_metric_catalog(&client, "requests").await;
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
    let app = crate::support::seed_app(&client, &name, &plan_id).await;

    // Usage = i64::MAX requests in the current period.
    seed_requests(&client, &app, i64::MAX as u64).await;

    // The sweep must NOT transition (skip) our app, and write NO state row for it.
    engine.evaluate_all().await.expect("evaluate_all");
    let (state, hist) = read_state(&client, &app).await;
    assert_eq!(
        state, None,
        "no app_spend_state row written for the skipped app"
    );
    assert_eq!(hist, 0, "no spend_state_history row for the skipped app");
}

#[compio::test(crate = "crate::support::live")]
async fn raising_limit_recovers_block_immediately() {
    // Faithful PG exercise of the raised-limit recovery: an app pinned at Block
    // recovers to Allow on the next tick once `set_limit` raises the cap far
    // above the deadband (deadband would otherwise hold Block at a fixed cap).
    let url = db_url();
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let engine = SpendEngine::new(registry);

    let (_plan, app) = make_app_on_priced_plan(&client, 100).await;
    seed_requests(&client, &app, 100).await;

    // Tick 1: Block.
    engine.evaluate_all().await.unwrap();
    assert_eq!(read_state(&client, &app).await.0.as_deref(), Some("block"));

    // Raise the override to 100_000 cents → 100 cents is now 0.1% → Allow.
    engine.set_limit(&app, Some(100_000)).await.unwrap();

    // Tick 2: immediate recovery to Allow (deadband bypassed by the limit
    // change), and a second history row recording Block → Allow.
    engine.evaluate_all().await.unwrap();
    let (state, hist) = read_state(&client, &app).await;
    assert_eq!(state.as_deref(), Some("allow"));
    assert_eq!(hist, 2, "Block\u{2192}Allow appends a second history row");
}

// ---------------------------------------------------------------------------
// Redesign regression (change 2): `set_limit` UPSERTs the CONFIG table
// `app_spend_limit` ONLY (not the derived `app_spend_state`), and the fleet
// evaluation reflects that override; AND every transition's
// `spend_state_history` row carries a NON-NULL `period` (the column is NOT NULL
// in the redesigned schema).
// ---------------------------------------------------------------------------

#[compio::test(crate = "crate::support::live")]
async fn set_limit_writes_only_app_spend_limit_and_fleet_eval_reflects_it() {
    let url = db_url();
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let engine = SpendEngine::new(registry);

    // Plan default = 100c. Ingest 100 requests = 100c ⇒ at the plan default this
    // is 100% ⇒ Block.
    let (_plan, app) = make_app_on_priced_plan(&client, 100).await;
    seed_requests(&client, &app, 100).await;

    // Set a generous override BEFORE the first eval. It must land in the dedicated
    // CONFIG table `app_spend_limit` — NOT in `app_spend_state` (which has no
    // override column anymore).
    engine.set_limit(&app, Some(100_000)).await.unwrap();

    // The override is in app_spend_limit…
    let override_row = client
        .query(
            "SELECT spend_limit_cents FROM zeroship.app_spend_limit WHERE app_id = $1",
            &[&app.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(
        override_row.len(),
        1,
        "set_limit wrote an app_spend_limit row"
    );
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

#[compio::test(crate = "crate::support::live")]
async fn transition_history_row_binds_non_null_period() {
    let url = db_url();
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let engine = SpendEngine::new(registry);

    // 100c cap, 100 requests ⇒ Block (a transition that writes history).
    let (_plan, app) = make_app_on_priced_plan(&client, 100).await;
    seed_requests(&client, &app, 100).await;
    engine.evaluate_all().await.unwrap();

    // `spend_state_history.period` is NOT NULL in the redesigned schema — the
    // INSERT MUST bind it (this is why the 0041 DDL + the code landed together).
    // Reading the bound period back as a DATE proves the bind. (RED if the code
    // omitted the period bind: the INSERT would fail the NOT NULL, so no row.)
    let rows = client
        .query(
            "SELECT period::date AS period FROM zeroship.spend_state_history \
             WHERE app_id = $1 ORDER BY at DESC LIMIT 1",
            &[&app.as_str()],
        )
        .await
        .expect("read history period");
    assert_eq!(rows.len(), 1, "the transition wrote a history row");
    let period: chrono::NaiveDate = rows[0].get("period");
    use chrono::Datelike;
    assert_eq!(
        period.day(),
        1,
        "the history period is a first-of-month billing_period DATE"
    );
}

/// A fleet sweep must not abort when an app is deleted between the app-list
/// scan and that app's state read. In production the deletion funnel runs
/// concurrently with the spend cron, so an app can be gone by the time the
/// sweep reaches it; the sweep owes every other app a verdict regardless.
///
/// THE INTERLEAVING IS FORCED, not raced: the subject holds `zeroship.plans`
/// exclusively so the sweep blocks in `PlanCatalog::list` - which runs AFTER it
/// has already read the app ids on its own connection - then deletes the app and
/// releases. `pg_stat_activity` proves the sweep is the backend waiting on that
/// lock before the delete happens.
#[compio::test(crate = "crate::support::live")]
async fn evaluate_all_skips_an_app_deleted_mid_sweep() {
    use std::time::Duration;

    let url = db_url();
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let engine = SpendEngine::new(registry);
    crate::support::seed_pricing_catalog(&client).await;

    let (_plan, app) = make_app_on_priced_plan(&client, 100).await;
    seed_requests(&client, &app, 100).await;

    // A side session that holds `plans` exclusively for the duration of the
    // interleaving. The sweep reads its app list first, then blocks in
    // `catalog.list` until this session commits.
    let (holder, holder_conn) = connect(&url, NoTls).await.expect("holder connect");
    crate::support::live::spawn(async move {
        let _ = holder_conn.run().await;
    });
    let holder_pid: i32 = holder
        .query("SELECT pg_backend_pid() AS pid", &[])
        .await
        .expect("holder pid")[0]
        .get("pid");
    holder
        .batch_execute("BEGIN; LOCK TABLE zeroship.plans IN ACCESS EXCLUSIVE MODE")
        .await
        .expect("hold plans exclusively");

    let sweep = compio::runtime::spawn(async move { engine.evaluate_all().await });

    let mut blocked = false;
    for _ in 0..200 {
        let waiting: bool = client
            .query(
                "SELECT EXISTS ( \
                   SELECT 1 FROM pg_stat_activity a \
                     JOIN pg_locks l ON l.pid = a.pid \
                    WHERE NOT l.granted AND a.wait_event_type = 'Lock' \
                      AND a.query LIKE '%FROM zeroship.plans ORDER BY id%' \
                      AND $1 = ANY(pg_blocking_pids(a.pid))) AS waiting",
                &[&holder_pid],
            )
            .await
            .expect("poll pg_locks")[0]
            .get("waiting");
        if waiting {
            blocked = true;
            break;
        }
        compio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        blocked,
        "the sweep must reach its per-app read only after it has scanned the app list; \
         it never appeared waiting on the plans lock"
    );

    // The app-list scan is behind us, so deleting the row now is exactly the
    // production race: the sweep will reach its state read and find no row.
    let deleted = holder
        .execute(
            "DELETE FROM zeroship.apps WHERE id = $1",
            &[&app.as_str()],
        )
        .await
        .expect("delete the app mid-sweep");
    assert_eq!(deleted, 1, "the subject app is gone");
    holder.batch_execute("COMMIT").await.expect("release plans");

    let outcome = sweep
        .await
        .expect("the sweep task did not panic")
        .expect("a vanished app is skipped, not a sweep-fatal error");
    assert!(
        !outcome.iter().any(|t| t.app_id == app),
        "the deleted app cannot have transitioned",
    );
}

/// Two concurrent `evaluate_all` drives that both read the same prior state must
/// append EXACTLY ONE transition row. `persist_transition` arbitrates with a
/// compare-and-swap on the stored state; without it, both drives append history
/// for one `allow`->`block` transition.
///
/// THE RACE IS FORCED, not raced: a side session pre-creates the app's state row
/// at `allow` and holds a row lock on it, so both drives read `allow` (a plain
/// SELECT does not block) and then block at the write. Releasing lets them race;
/// the durable history count must be one.
///
/// Runs alone in a child process: the sweep is fleet-wide, so a sibling case's
/// sweep could transition this app first and write the one history (and,
/// best-effort, audit) row itself, hiding whether THIS case's drives arbitrated.
/// The spawner below runs it in a child whose own database no sibling observes.
#[compio::test(crate = "crate::support::live")]
#[ignore = "shares the fleet-wide app/state space with siblings; its spawner runs it alone in a child"]
async fn concurrent_evaluate_all_appends_one_history_row() {
    use std::time::Duration;

    let url = db_url();
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let engine = SpendEngine::new(registry);
    crate::support::seed_pricing_catalog(&client).await;

    let (_plan, app) = make_app_on_priced_plan(&client, 100).await;
    seed_requests(&client, &app, 100).await;

    // Pre-create the row at `allow` so both drives' prior-state reads see it.
    let period = current_period_start_unix();
    client
        .execute(
            "INSERT INTO zeroship.app_spend_state \
               (app_id, state, spend_cents, eval_limit_cents, period, evaluated_at) \
             VALUES ($1, 'allow', 0, 0, $2::date, NOW())",
            &[&app.as_str(), &crate::support::period_date(period)],
        )
        .await
        .expect("pre-create allow state");

    // A side session holds a row lock on that state row. Both drives read `allow`
    // and block at the compare-and-swap write.
    let (holder, holder_conn) = connect(&url, NoTls).await.expect("holder connect");
    crate::support::live::spawn(async move {
        let _ = holder_conn.run().await;
    });
    holder.batch_execute("BEGIN").await.expect("begin holder tx");
    holder
        .query(
            "SELECT 1 FROM zeroship.app_spend_state WHERE app_id = $1 FOR UPDATE",
            &[&app.as_str()],
        )
        .await
        .expect("lock the state row");

    let first = engine.clone();
    let second = engine.clone();
    let a = compio::runtime::spawn(async move { first.evaluate_all().await });
    let b = compio::runtime::spawn(async move { second.evaluate_all().await });

    let mut raced = false;
    for _ in 0..400 {
        let waiting: i64 = client
            .query(
                "SELECT COUNT(*)::bigint AS n FROM pg_stat_activity a \
                  WHERE a.wait_event_type = 'Lock' \
                    AND a.query LIKE '%INSERT INTO zeroship.app_spend_state%' \
                    AND cardinality(pg_blocking_pids(a.pid)) > 0",
                &[],
            )
            .await
            .expect("count blocked drives")[0]
            .get("n");
        if waiting >= 2 {
            raced = true;
            break;
        }
        compio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        raced,
        "both drives must block at the state write after reading `allow`"
    );

    holder.batch_execute("COMMIT").await.expect("release the row");

    let _ = a.await.expect("drive a did not panic").expect("drive a ok");
    let _ = b.await.expect("drive b did not panic").expect("drive b ok");

    let (state, hist) = read_state(&client, &app).await;
    assert_eq!(state.as_deref(), Some("block"), "the app ended blocked");
    assert_eq!(
        hist, 1,
        "two concurrent drives of one allow->block transition append exactly one history row",
    );

    // The winning drive also wrote the transition's durable audit row. This pins
    // that the engine, not only the cron, audits a persisted transition, and that
    // the compare-and-swap admits one audit row under concurrent drives.
    let audit: i64 = client
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.app_audit \
              WHERE app_id = $1 AND action = 'spend_state_change'",
            &[&app.as_str()],
        )
        .await
        .expect("count spend audits")[0]
        .get("n");
    assert_eq!(
        audit, 1,
        "exactly one SpendStateChange audit row for our app across the concurrent drives",
    );
}

/// Run the forced concurrent-transition case alone in a child (see
/// [`crate::support::isolated_case`]) so no sibling fleet-wide sweep touches its app.
#[test]
fn concurrent_evaluate_all_appends_one_history_row_in_an_isolated_process() {
    crate::support::isolated_case::run_alone(
        "integration::spend::concurrent_evaluate_all_appends_one_history_row",
    );
}
