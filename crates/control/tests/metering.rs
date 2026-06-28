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
use zeroship_control::pricing::{per_metric_units, total_units};
use zeroship_control::pricing_store::PricingStore;
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

/// Metering coverage (#27, H2): the gateway is a SECOND metering producer.
/// Drive the FAITHFUL producer→ingest seam — a real `zeroship_metering::Meter`
/// records `gateway_egress_bytes`, the gateway drains it and builds a
/// `UsageReport` with a `gate-…` restart-unique producer id (exactly what the
/// gateway flush task does), then it ingests through the REAL `Metering` path
/// and aggregates into `usage_aggregates` under the route's app_id. A worker
/// report for the SAME app then sums alongside it (the two egress metrics are
/// disjoint, so "total egress" is their sum) — no double-count, no collision
/// between the `gate-` and worker producer ids.
#[compio::test]
async fn gateway_egress_report_aggregates_into_usage_aggregates() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let metering = Metering::new(registry);
    let app = make_app(&client).await;
    let period = period_start_unix(1_900_000_000);

    // --- Gateway producer side (the real Meter, the real producer id) ---
    let gate_meter = zeroship_metering::Meter::new();
    gate_meter.increment(&app.to_string(), "gateway_egress_bytes", 4096);
    let snapshot = gate_meter.drain();
    let gate_producer = zeroship_metering::boot_worker_id("gate-test-host");
    assert!(gate_producer.starts_with("gate-"), "gateway producer id prefix");
    let gate_report = zeroship_metering::build_report(&gate_producer, 1, snapshot)
        .expect("non-empty gateway snapshot builds a report");

    let out = metering
        .ingest_at(&gate_report, period)
        .await
        .expect("gateway report ingests");
    assert!(!out.duplicate, "first gateway report is fresh");

    // The gateway-owned egress landed under the right app_id + metric.
    assert_eq!(
        metering.total(&app, period, "gateway_egress_bytes").await.unwrap(),
        4096,
        "gateway_egress_bytes aggregated for the route's app",
    );

    // --- A worker report for the SAME app sums alongside (no collision) ---
    let worker_id = format!("worker-host-{}", Uuid::new_v4());
    metering
        .ingest_at(
            &report(&worker_id, 1, app, AppUsage { egress_bytes: 1000, ..Default::default() }),
            period,
        )
        .await
        .expect("worker report ingests");

    // The two egress metrics are DISJOINT — the worker's `egress_bytes` and the
    // gateway's `gateway_egress_bytes` coexist; "total egress" is their sum.
    assert_eq!(
        metering.total(&app, period, "gateway_egress_bytes").await.unwrap(),
        4096,
        "gateway egress unchanged by the worker report (disjoint metric)",
    );
    assert_eq!(
        metering.total(&app, period, "egress_bytes").await.unwrap(),
        1000,
        "worker egress recorded under its own metric (no double-count)",
    );
}

#[compio::test]
async fn native_socket_ingress_bills_once_with_net_ingress_attribution() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let metering = Metering::new(registry.clone());
    let app = make_app(&client).await;
    let period = period_start_unix(1_900_000_000);
    let worker = format!("w-{}", Uuid::new_v4());

    let mut custom = HashMap::new();
    custom.insert("net_ingress_bytes".to_string(), 10_000);
    metering
        .ingest_at(
            &report(
                &worker,
                1,
                app,
                AppUsage {
                    ingress_bytes: 10_000,
                    custom,
                    ..Default::default()
                },
            ),
            period,
        )
        .await
        .expect("ingest native socket ingress");

    let totals = metering.period_totals(&app, period).await.expect("period totals");
    assert_eq!(totals.get("ingress_bytes").copied(), Some(10_000));
    assert_eq!(
        totals.get("net_ingress_bytes").copied(),
        Some(10_000),
        "net_ingress_bytes remains an attribution metric"
    );

    let weights = PricingStore::new(registry).weights().await.expect("weights");
    let units = total_units(&weights, &totals).expect("price ingress totals");
    assert_eq!(
        units, 1,
        "10 KiB of successful socket ingress is billable once via ingress_bytes, not again via \
         net_ingress_bytes"
    );
    assert_eq!(
        weights
            .get("net_ingress_bytes")
            .expect("net_ingress_bytes weight row remains for attribution")
            .units_per_op,
        0,
        "net_ingress_bytes is present but unweighted"
    );
    assert_eq!(
        per_metric_units("net_ingress_bytes", 10_000, &weights).expect("net ingress units"),
        0,
        "net_ingress_bytes must stay cataloged but attribution-only"
    );
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

// ---------------------------------------------------------------------------
// Redesign regression (change 1): the dedup key stays (worker_id, sequence) —
// NEVER (worker_id, sequence, period) — so a retried report straddling the UTC
// month boundary cannot pass the gate a second time and DOUBLE-APPLY.
// ---------------------------------------------------------------------------

#[compio::test]
async fn month_boundary_retry_does_not_double_apply() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let metering = Metering::new(registry);
    let app = make_app(&client).await;
    let worker = format!("w-{}", Uuid::new_v4());

    // Use two periods that are CALENDAR-MONTH apart so they map to DIFFERENT
    // `billing_period` DATE buckets. The retry presents the SAME (worker, seq).
    let june = period_start_unix(
        chrono::TimeZone::timestamp_opt(&chrono::Utc, 1_900_000_000, 0)
            .single()
            .unwrap()
            .timestamp(),
    );
    let july = {
        use chrono::{Datelike, TimeZone};
        let dt = chrono::Utc.timestamp_opt(june, 0).single().unwrap();
        let (y, m) = if dt.month() == 12 { (dt.year() + 1, 1) } else { (dt.year(), dt.month() + 1) };
        chrono::Utc.with_ymd_and_hms(y, m, 1, 0, 0, 0).single().unwrap().timestamp()
    };

    // First apply lands in the June bucket.
    let out1 = metering
        .ingest_at(&report(&worker, 1, app, AppUsage { requests: 10, ..Default::default() }), june)
        .await
        .expect("ingest june");
    assert!(!out1.duplicate, "first sight applies");
    assert_eq!(metering.total(&app, june, "requests").await.unwrap(), 10);

    // The SAME (worker, sequence) retried, now bucketed to JULY (month boundary).
    // The dedup gate keys ONLY on (worker_id, sequence), so this is a DUPLICATE —
    // it must NOT apply to the July bucket. (RED if `period` were in the dedup
    // PK: the new (w,seq,july) key would pass and double-apply 10 to July.)
    let out2 = metering
        .ingest_at(&report(&worker, 1, app, AppUsage { requests: 10, ..Default::default() }), july)
        .await
        .expect("ingest july retry");
    assert!(out2.duplicate, "the month-boundary retry is a DUPLICATE (dedup keys on worker,seq only)");
    assert_eq!(
        metering.total(&app, july, "requests").await.unwrap(),
        0,
        "the month-boundary retry did NOT double-apply into the July bucket",
    );
    assert_eq!(metering.total(&app, june, "requests").await.unwrap(), 10, "June bucket unchanged");
}

// ---------------------------------------------------------------------------
// Redesign regression (change 1): a custom metric REFUSED at the per-app cap is
// dropped-with-warn — it does NOT FK-abort the report. The platform/primitive
// metrics (always cataloged) AND already-registered custom metrics still apply.
// ---------------------------------------------------------------------------

#[compio::test]
async fn capped_custom_metric_is_dropped_without_fk_aborting_the_report() {
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

    // Fill the app's custom-metric catalog to EXACTLY the cap by pre-seeding cap
    // rows directly (faithful: same shape the ingest path writes).
    let cap = zeroship_control::metering::MAX_CUSTOM_METRICS_PER_APP;
    for i in 0..cap {
        client
            .execute(
                "INSERT INTO zeroship.billing_metrics (metric, kind, unit, owner_app, last_seen_at) \
                 VALUES ($1, 'custom', 'unit', $2, NOW())",
                &[&format!("cap_fill_{}_{i}", app.simple()), &app],
            )
            .await
            .expect("seed cap-fill metric");
    }

    // A report carrying BOTH a platform metric (requests, always cataloged) AND a
    // BRAND-NEW custom metric that would exceed the cap. The over-cap metric must
    // be dropped-with-warn (NOT registered, NOT applied) and MUST NOT FK-abort the
    // whole report — so `requests` still lands. (RED if the ingest applied the
    // over-cap delta: the usage_aggregates.metric FK to billing_metrics would
    // RESTRICT-abort the entire report tx, losing the `requests` delta too.)
    let mut custom = HashMap::new();
    let over_cap = format!("over_cap_{}", Uuid::new_v4().simple());
    custom.insert(over_cap.clone(), 7u64);
    metering
        .ingest_at(
            &report(&worker, 1, app, AppUsage { requests: 42, custom, ..Default::default() }),
            period,
        )
        .await
        .expect("ingest must succeed (the over-cap metric is dropped, not FK-aborting)");

    // The platform metric applied; the over-cap custom metric did NOT.
    assert_eq!(
        metering.total(&app, period, "requests").await.unwrap(),
        42,
        "the platform metric still applied (the report was not FK-aborted)",
    );
    assert_eq!(
        metering.total(&app, period, &over_cap).await.unwrap(),
        0,
        "the over-cap custom metric was dropped (never applied)",
    );
    // And it was NOT registered in the catalog.
    let cataloged = client
        .query(
            "SELECT 1 FROM zeroship.billing_metrics WHERE metric = $1",
            &[&over_cap],
        )
        .await
        .expect("query catalog");
    assert!(cataloged.is_empty(), "the refused custom metric was NOT registered (capped)");
}
