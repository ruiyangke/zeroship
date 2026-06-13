//! Integration tests for the metering ingest pipeline against a live
//! Postgres. Exercises the REAL `Metering` path (idempotent ingest +
//! period aggregation over `zeroship.usage_aggregates` /
//! `zeroship.usage_reports_seen`), not the pure-logic `IngestLedger` (that
//! is unit-tested in `src/metering.rs`).
//!
//! Set `CONTROL_TEST_DB` to run; tests silently skip otherwise (CI without
//! a DB stays green). The pure-logic dedup/aggregation/rollover properties
//! are covered DB-free by the `src/metering.rs` unit tests; these assert the
//! same properties survive the real SQL (the UPSERT + the ON CONFLICT DO
//! NOTHING dedup gate).

use std::collections::HashMap;

use compio_postgres::{connect, NoTls};
use uuid::Uuid;

use zeroship_control::metering::{period_start_unix, Metering};
use zeroship_control::Registry;
use zeroship_core::types::{AppUsage, UsageReport};

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

/// Insert a bare `apps` row directly so the FK on `usage_aggregates(app_id)`
/// is satisfied without the full create_app owner/user dance. Returns the id.
async fn make_app(client: &compio_postgres::Client) -> Uuid {
    // PR4: apps.plan_id is an FK into zeroship.plans. Ensure the built-in free
    // plan exists, then reference its real catalog id.
    let free = zeroship_control::bootstrap_console::free_plan_id();
    client
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'free', 0, 0, NULL, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', 0) \
             ON CONFLICT (id) DO NOTHING",
            &[&free],
        )
        .await
        .expect("seed free plan");
    let name = format!("metering-test-{}", Uuid::new_v4());
    let rows = client
        .query(
            "INSERT INTO zeroship.apps (name, plan_id, api_key, api_key_hash) \
             VALUES ($1, $2, $3, '') RETURNING id",
            &[&name, &free, &Uuid::new_v4().to_string()],
        )
        .await
        .expect("insert app");
    rows[0].get("id")
}

fn report(worker: &str, seq: u64, app: Uuid, usage: AppUsage) -> UsageReport {
    let mut counters = HashMap::new();
    counters.insert(app, usage);
    UsageReport {
        worker_id: worker.to_string(),
        report_id: Uuid::now_v7(),
        sequence: seq,
        counters,
    }
}

#[compio::test]
async fn ingest_aggregates_into_usage_aggregates() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let metering = Metering::new(registry);
    let app = make_app(&client).await;
    let period = period_start_unix(1_900_000_000); // a fixed period for the test

    let worker = format!("w-{}", Uuid::new_v4());
    let mut custom = HashMap::new();
    custom.insert("emails_sent".to_string(), 4u64);

    let out1 = metering
        .ingest_at(
            &report(&worker, 1, app, AppUsage { requests: 10, custom: custom.clone(), ..Default::default() }),
            period,
        )
        .await
        .expect("ingest 1");
    assert!(!out1.duplicate);
    assert_eq!(out1.high_water_sequence, 1);

    metering
        .ingest_at(&report(&worker, 2, app, AppUsage { requests: 5, ..Default::default() }), period)
        .await
        .expect("ingest 2");

    assert_eq!(metering.total(&app, period, "requests").await.unwrap(), 15);
    assert_eq!(metering.total(&app, period, "emails_sent").await.unwrap(), 4);
}

#[compio::test]
async fn duplicate_report_does_not_double_count_in_pg() {
    // The key idempotency property against the REAL ON CONFLICT DO NOTHING
    // dedup gate: re-ingesting the SAME (worker_id, sequence) must not add
    // its deltas a second time.
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let metering = Metering::new(registry);
    let app = make_app(&client).await;
    let period = period_start_unix(1_900_000_000);
    let worker = format!("w-{}", Uuid::new_v4());

    let r = report(&worker, 1, app, AppUsage { requests: 10, ..Default::default() });

    let first = metering.ingest_at(&r, period).await.expect("first ingest");
    assert!(!first.duplicate, "first ingest is not a duplicate");

    let second = metering.ingest_at(&r, period).await.expect("duplicate ingest");
    assert!(second.duplicate, "exact retry must be flagged a duplicate");
    assert_eq!(second.high_water_sequence, 1, "high-water unchanged by duplicate");

    assert_eq!(
        metering.total(&app, period, "requests").await.unwrap(),
        10,
        "duplicate must NOT double-count (would be 20 without the dedup gate)"
    );
}

#[compio::test]
async fn worker_restart_does_not_drop_post_restart_usage() {
    // Restart-safety regression (the silent under-billing bug): the dedup key
    // is (worker_id, sequence) and the per-process SequenceSource resets to 1
    // every boot. If worker_id were STABLE across restarts (as $HOSTNAME is),
    // a post-restart report re-emitting sequence 1 would collide with the
    // pre-restart row in usage_reports_seen → ON CONFLICT DO NOTHING drops it
    // → post-restart usage silently lost.
    //
    // The fix folds a per-process boot nonce into the metering identity
    // (`boot_worker_id`), so the two incarnations carry DIFFERENT worker_ids
    // even though both restart their sequence at 1. This test models the two
    // boots with two distinct identities sharing the same stable base.
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let metering = Metering::new(registry);
    let app = make_app(&client).await;
    let period = period_start_unix(1_900_000_000);

    // A stable logical base (think $HOSTNAME) shared by both boots. Each boot
    // mints a restart-unique identity by folding a fresh per-process nonce
    // onto the base — exactly what `zeroship_metering::boot_worker_id` does
    // (kept as a literal here so the control test needs no dep on the worker-
    // side metering crate). The control-side property under test is that two
    // distinct identities sharing a base do NOT collide in the dedup ledger.
    let base = format!("pod-{}", Uuid::new_v4());
    let boot_a = format!("{base}-{}", Uuid::new_v4());
    let boot_b = format!("{base}-{}", Uuid::new_v4());
    assert_ne!(boot_a, boot_b, "restart must mint a fresh metering identity");

    // Boot A: emit sequence 1 with 10 requests.
    let a1 = metering
        .ingest_at(&report(&boot_a, 1, app, AppUsage { requests: 10, ..Default::default() }), period)
        .await
        .expect("boot A seq 1");
    assert!(!a1.duplicate, "boot A seq 1 is fresh");

    // --- worker restart: sequence resets to 1, fresh identity (boot_b) ---
    // Boot B re-emits sequence 1 with NEW deltas. Pre-fix this collided with
    // boot A's (stable_id, 1) and was dropped. Post-fix it must apply.
    let b1 = metering
        .ingest_at(&report(&boot_b, 1, app, AppUsage { requests: 7, ..Default::default() }), period)
        .await
        .expect("boot B seq 1");
    assert!(
        !b1.duplicate,
        "post-restart report must NOT be dropped as a phantom duplicate"
    );

    // Both boots' deltas must be billed: 10 (boot A) + 7 (boot B) = 17.
    assert_eq!(
        metering.total(&app, period, "requests").await.unwrap(),
        17,
        "post-restart usage must be applied, not silently lost"
    );

    // Exactly-once still holds WITHIN a process: a genuine in-process retry of
    // boot B's sequence 1 (same identity + sequence) is still a duplicate.
    let b1_retry = metering
        .ingest_at(&report(&boot_b, 1, app, AppUsage { requests: 7, ..Default::default() }), period)
        .await
        .expect("boot B seq 1 retry");
    assert!(b1_retry.duplicate, "in-process retransmit is still deduped");
    assert_eq!(
        metering.total(&app, period, "requests").await.unwrap(),
        17,
        "in-process duplicate must not double-count"
    );
}

#[compio::test]
async fn month_rollover_lands_in_separate_period_rows() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let metering = Metering::new(registry);
    let app = make_app(&client).await;
    let worker = format!("w-{}", Uuid::new_v4());

    // Two instants in different UTC months → different period_start buckets.
    let june = period_start_unix(
        chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 6, 15, 0, 0, 0)
            .unwrap()
            .timestamp(),
    );
    let july = period_start_unix(
        chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 7, 15, 0, 0, 0)
            .unwrap()
            .timestamp(),
    );
    assert_ne!(june, july);

    metering
        .ingest_at(&report(&worker, 1, app, AppUsage { requests: 10, ..Default::default() }), june)
        .await
        .unwrap();
    metering
        .ingest_at(&report(&worker, 2, app, AppUsage { requests: 7, ..Default::default() }), july)
        .await
        .unwrap();

    assert_eq!(metering.total(&app, june, "requests").await.unwrap(), 10);
    assert_eq!(metering.total(&app, july, "requests").await.unwrap(), 7);
}

#[compio::test]
async fn current_period_totals_returns_metric_map() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let metering = Metering::new(registry);
    let app = make_app(&client).await;
    let worker = format!("w-{}", Uuid::new_v4());

    let mut custom = HashMap::new();
    custom.insert("widgets".to_string(), 9u64);
    metering
        .ingest(&report(&worker, 1, app, AppUsage { requests: 3, custom, ..Default::default() }))
        .await
        .unwrap();

    let totals = metering.current_period_totals(&app).await.unwrap();
    assert_eq!(totals.get("requests").copied(), Some(3));
    assert_eq!(totals.get("widgets").copied(), Some(9));
}
