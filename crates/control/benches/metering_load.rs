//! Metering / billing hot-path load + contention MEASUREMENT (#29).
//!
//! A re-runnable harness that drives the REAL control-plane metering paths
//! against a live Postgres and reports MEASURED numbers — never estimates.
//! It targets the scaling risk the billing design pre-staged-but-deferred:
//! the `usage_aggregates` hot-row UPSERT contention under high-volume ingest.
//!
//! It exercises THREE paths, all through the real public entry points (no
//! reimplementation of the production SQL where a public fn exists):
//!
//!   1. `ingest` — drive `Metering::ingest_at` (the real per-report tx: the
//!      `(worker_id, sequence)` dedup INSERT + the `(app_id, period, metric)`
//!      `usage_aggregates` UPSERT) at high concurrency, contrasting a FEW hot
//!      apps (worst-case row-lock contention on a handful of PK rows) against
//!      MANY apps (contention spread across many rows). Reports reports/sec and
//!      per-tx latency p50/p95/p99.
//!   2. `spend` — drive the real `SpendEngine::evaluate_all` fleet sweep over
//!      N synthetic apps with usage; reports the sweep wall time (the
//!      bulk-prefilter / one-conn-per-app perf fix at scale).
//!   3. `reconcile` — measure the month-close reconcile READ pattern over
//!      N creators × M apps with a controllable ACTIVE fraction, to confirm the
//!      cost is O(active) not O(all-apps-ever). Uses the REAL owner-grouping +
//!      active-app prefilter SQL `billing_reconcile::sweep` runs and the REAL
//!      `Metering::period_totals_on` per-app read. The per-app plan lookup is
//!      the one-line `SELECT plan_id FROM apps WHERE id=$1` (the body of the
//!      `pub(crate)` `lookup_plan_id_on`, inlined because it is not callable
//!      from a bench crate). The Stripe POST + invoice writes are deliberately
//!      OUT of scope here — they are a fixed per-active-CREATOR network cost,
//!      not part of the "scales with all-apps-ever" question this measures.
//!
//! ## How to run
//!
//! Requires a dedicated, migrated Postgres database. DO NOT point this at the
//! real `zeroship` DB or `zeroship_billing_test`.
//!
//! ```bash
//! # 1. create + migrate a dedicated DB (one time):
//! createdb -h localhost -p 5440 -U postgres zeroship_metering_load
//! docker run --rm --network host -v "$PWD/db/changelog:/liquibase/changelog:ro" \
//!   liquibase/liquibase:4.31 \
//!   --url=jdbc:postgresql://localhost:5440/zeroship_metering_load \
//!   --username=postgres --password=zeroship \
//!   --changelog-file=changelog/db.changelog-master.yaml \
//!   --liquibase-schema-name=public update
//!
//! # 2. run (the env var both gates AND points the harness):
//! METERING_LOAD_DB='postgres://postgres:zeroship@localhost:5440/zeroship_metering_load' \
//!   cargo bench -p zeroship-control --bench metering_load -- all
//! ```
//!
//! Sub-commands: `ingest`, `spend`, `reconcile`, or `all` (default). Without
//! `METERING_LOAD_DB` set the harness prints a how-to-run note and exits 0 (so
//! a CI `cargo bench` without a DB stays green). The harness is self-cleaning:
//! every run seeds into FRESH app/creator ids and truncates its own working
//! rows on entry (it never touches rows it did not create within the run).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use compio_postgres::{Client, NoTls};
use uuid::Uuid;

use zeroship_control::metering::{period_start_unix, Metering};
use zeroship_control::spend::SpendEngine;
use zeroship_control::Registry;
use zeroship_core::types::{AppUsage, UsageReport};

// A FIXED unix instant → a stable calendar-month period bucket for the whole
// run. Mid-2030 so it never collides with a real current-period sweep.
const BENCH_PERIOD_INSTANT: i64 = 1_900_000_000; // 2030-03-17 UTC

/// The first-of-month `billing_period` DATE for a unix-seconds period start —
/// the key every period-keyed billing table uses. This mirrors the `pub(crate)`
/// `metering::period_date` (not callable from a bench crate), re-derived here so
/// the bench owns its key shape. Same computation: midnight UTC on the 1st.
fn period_date(period_start_unix_secs: i64) -> chrono::NaiveDate {
    use chrono::{Datelike, TimeZone};
    let dt = chrono::Utc
        .timestamp_opt(period_start_unix_secs, 0)
        .single()
        .unwrap_or_else(chrono::Utc::now);
    chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), 1).expect("valid first-of-month")
}

fn main() {
    let Ok(db_url) = std::env::var("METERING_LOAD_DB") else {
        print_skip_note();
        return;
    };
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "all".to_string());

    let rt = compio::runtime::Runtime::new().expect("compio runtime");
    rt.block_on(async move {
        let registry = Registry::new(&db_url).await.expect("registry connect");
        let admin = connect(&db_url).await;
        seed_prereqs(&admin).await;
        // Self-cleaning: wipe the harness's working tables on entry so a re-run
        // starts from a known-empty state (and any residue an interrupted prior
        // run left — e.g. the ingest scenarios' fresh apps — is cleared). Safe
        // ONLY because this is the DEDICATED `zeroship_metering_load` DB; the
        // guard refuses to run if the URL does not name that database.
        truncate_working_tables(&db_url, &admin).await;

        match cmd.as_str() {
            "ingest" => bench_ingest(&registry, &admin).await,
            "spend" => bench_spend(&registry, &admin).await,
            "reconcile" => bench_reconcile(&db_url, &admin).await,
            "all" => {
                bench_ingest(&registry, &admin).await;
                bench_spend(&registry, &admin).await;
                bench_reconcile(&db_url, &admin).await;
            }
            other => {
                eprintln!("unknown sub-command {other:?}; use ingest|spend|reconcile|all");
                std::process::exit(2);
            }
        }
    });
}

fn print_skip_note() {
    println!("metering_load: METERING_LOAD_DB not set — skipping (this is expected under a");
    println!("plain `cargo bench`). To run the measurement, point it at a DEDICATED migrated DB:");
    println!();
    println!("  METERING_LOAD_DB='postgres://postgres:zeroship@localhost:5440/zeroship_metering_load' \\");
    println!("    cargo bench -p zeroship-control --bench metering_load -- all");
    println!();
    println!("Do NOT use the real `zeroship` DB or `zeroship_billing_test`. See the file");
    println!("header for the createdb + Liquibase migration steps.");
}

// ===========================================================================
// Shared seeding / connection helpers.
// ===========================================================================

async fn connect(db_url: &str) -> Client {
    let (client, conn) = compio_postgres::connect(db_url, NoTls)
        .await
        .expect("pg connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

/// Ensure the built-in catalog prereqs the spend/reconcile pricing reads need
/// exist: a free plan, the `requests` metric weight (1 CU/op), and a global
/// default FX. The platform metrics (`requests`, `cpu_us`, …) are seeded by the
/// changelog, so ingest of the fixed counters never FK-aborts.
async fn seed_prereqs(admin: &Client) {
    admin
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ('pln_metering_load', 'metering-load', 0, 0, 1000000000000, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', 100000) \
             ON CONFLICT (id) DO NOTHING",
            &[],
        )
        .await
        .expect("seed plan");
    admin
        .execute(
            "INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units) \
             VALUES ('requests', 1, 1) \
             ON CONFLICT (metric) DO UPDATE SET units_per_op = 1, per_units = 1",
            &[],
        )
        .await
        .expect("seed requests weight");
    // A global default FX so spend/reconcile pricing never fails-closed.
    admin
        .execute(
            "INSERT INTO zeroship.pricing_config (scope, default_fx_pico_cents_per_unit, updated_at) \
             VALUES ('global', 30000000, NOW()) \
             ON CONFLICT (scope) DO NOTHING",
            &[],
        )
        .await
        .ok();
}

/// Wipe the harness's working rows so a re-run starts clean (idempotent +
/// self-cleaning). REFUSES to run unless the connection URL names the dedicated
/// `zeroship_metering_load` database — a guard so a mis-pointed run can never
/// truncate the real `zeroship` DB or `zeroship_billing_test`.
async fn truncate_working_tables(db_url: &str, admin: &Client) {
    assert!(
        db_url.contains("zeroship_metering_load"),
        "refusing to truncate: METERING_LOAD_DB must name the dedicated \
         `zeroship_metering_load` database (got {db_url:?})"
    );
    // FK-safe order via TRUNCATE … CASCADE on the working tables. These are the
    // ONLY tables the harness writes; the seeded catalog (plans, metric_weights,
    // pricing_config, billing_metrics) is left intact.
    admin
        .execute(
            "TRUNCATE zeroship.usage_aggregates, zeroship.usage_reports_seen, \
                      zeroship.app_spend_state, zeroship.app_spend_limit, \
                      zeroship.app_members, zeroship.apps RESTART IDENTITY CASCADE",
            &[],
        )
        .await
        .expect("truncate working tables");
    // `users` are referenced by app_members (already truncated via CASCADE on
    // apps) — clear the harness's synthetic creators too.
    admin
        .execute("DELETE FROM zeroship.users", &[])
        .await
        .ok();
}

/// Insert a bare `apps` row on the load plan. Returns the id. (Same shape the
/// `metering.rs` integration test's `make_app` uses, minus the owner dance.)
async fn make_app(admin: &Client) -> Uuid {
    let name = format!("ml-app-{}", Uuid::new_v4());
    let rows = admin
        .query(
            "INSERT INTO zeroship.apps (name, plan_id, api_key, api_key_hash) \
             VALUES ($1, 'pln_metering_load', $2, '') RETURNING id",
            &[&name, &Uuid::new_v4().to_string()],
        )
        .await
        .expect("insert app");
    rows[0].get("id")
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

/// p50/p95/p99 (ms) from a set of per-op durations.
fn percentiles(mut durs: Vec<Duration>) -> (f64, f64, f64, f64) {
    durs.sort_unstable();
    let n = durs.len();
    let pick = |p: f64| -> f64 {
        if n == 0 {
            return 0.0;
        }
        let idx = ((p * (n as f64 - 1.0)).round() as usize).min(n - 1);
        durs[idx].as_secs_f64() * 1000.0
    };
    let mean = if n == 0 {
        0.0
    } else {
        durs.iter().map(|d| d.as_secs_f64()).sum::<f64>() / n as f64 * 1000.0
    };
    (pick(0.50), pick(0.95), pick(0.99), mean)
}

// ===========================================================================
// Bench 1 — ingest throughput + hot-row UPSERT contention (the headline).
// ===========================================================================

/// One ingest scenario: `apps` distinct hot rows, `concurrency` in-flight tasks
/// each looping `per_task` reports across those apps.
struct IngestScenario {
    label: &'static str,
    apps: usize,
    concurrency: usize,
    per_task: usize,
}

async fn bench_ingest(registry: &Registry, admin: &Client) {
    println!("\n================================================================");
    println!("BENCH 1 — usage_aggregates ingest throughput + hot-row contention");
    println!("================================================================");
    println!(
        "Each task calls the REAL Metering::ingest_at (per-report tx: dedup INSERT\n\
         + usage_aggregates UPSERT). HOT = few apps (heavy row-lock contention on\n\
         a handful of PK rows); SPREAD = one app per task (contention dispersed)."
    );
    println!(
        "\n{:<10} {:>6} {:>6} {:>9} {:>11} {:>9} {:>9} {:>9} {:>9}",
        "scenario", "apps", "conc", "reports", "reports/s", "p50ms", "p95ms", "p99ms", "meanms"
    );

    // A sweep of concurrency levels, each in HOT (1 app) and SPREAD (one app per
    // task) shape. IMPORTANT ENV CEILING: `Registry` opens a FRESH PG connection
    // per `ingest_at` call (no pool), and the dev PG is `max_connections=100`
    // (3 reserved, ~6 used by the co-resident stack). At high concurrency the
    // per-call connection churn + detached-driver teardown lag bumps that
    // ceiling and PG resets peers. We therefore cap concurrency at 48 — safely
    // under the ceiling — and report the plateau within that band. The ceiling
    // itself is a finding (see the results doc): the no-pool Registry model
    // bounds ingest concurrency by `max_connections`, independent of row-lock
    // contention. The CONCURRENCY env var overrides the top tier for probing.
    let top = std::env::var("CONCURRENCY")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(32);
    // HOT and SPREAD are INTERLEAVED at each concurrency so the contention
    // comparison is apples-to-apples on the same connection-pressure state (the
    // 1.5s inter-scenario settle lets the no-pool churn's detached drivers free
    // their PG backends, keeping each scenario under the `max_connections`
    // ceiling). The pair at a given `conc` is the headline: same throughput,
    // but HOT inflates tail latency faster (the row-lock serialization tax).
    let scenarios = [
        IngestScenario { label: "hot-1app", apps: 1, concurrency: 1, per_task: 400 },
        IngestScenario { label: "spread", apps: 4, concurrency: 4, per_task: 250 },
        IngestScenario { label: "hot-1app", apps: 1, concurrency: 4, per_task: 250 },
        IngestScenario { label: "hot-4app", apps: 4, concurrency: 4, per_task: 250 },
        IngestScenario { label: "spread", apps: 8, concurrency: 8, per_task: 200 },
        IngestScenario { label: "hot-1app", apps: 1, concurrency: 8, per_task: 200 },
        IngestScenario { label: "hot-4app", apps: 4, concurrency: 8, per_task: 200 },
        IngestScenario { label: "spread", apps: 16, concurrency: 16, per_task: 150 },
        IngestScenario { label: "hot-1app", apps: 1, concurrency: 16, per_task: 150 },
        IngestScenario { label: "hot-4app", apps: 4, concurrency: 16, per_task: 150 },
        IngestScenario { label: "spread", apps: top, concurrency: top, per_task: 100 },
        IngestScenario { label: "hot-1app", apps: 1, concurrency: top, per_task: 100 },
        IngestScenario { label: "hot-4app", apps: 4, concurrency: top, per_task: 100 },
    ];

    for sc in &scenarios {
        run_ingest_scenario(registry, admin, sc).await;
    }
}

async fn run_ingest_scenario(registry: &Registry, admin: &Client, sc: &IngestScenario) {
    // Fresh apps per scenario so a prior scenario's accumulated totals / dedup
    // rows never interact with this one.
    let mut app_ids = Vec::with_capacity(sc.apps);
    for _ in 0..sc.apps {
        app_ids.push(make_app(admin).await);
    }
    let period = period_start_unix(BENCH_PERIOD_INSTANT);

    let mut tasks = Vec::with_capacity(sc.concurrency);
    let started = Instant::now();
    for t in 0..sc.concurrency {
        let registry = registry.clone();
        // Each task targets ONE app (round-robin) — so HOT (apps=1) makes every
        // task hammer the SAME usage_aggregates row, while SPREAD (apps=conc)
        // gives each task its own row. A restart-unique worker id per task keeps
        // every report a fresh (worker_id, sequence) — no dedup no-ops.
        let app = app_ids[t % app_ids.len()];
        let worker = format!("ml-w-{}-{}", t, Uuid::new_v4());
        let per_task = sc.per_task;
        let task = compio::runtime::spawn(async move {
            let metering = Metering::new(registry);
            let mut lats = Vec::with_capacity(per_task);
            let mut errs = 0usize;
            for seq in 0..per_task {
                let r = report(&worker, seq as u64, app, 1);
                let op = Instant::now();
                // Errors are COUNTED, not fatal: at high concurrency the no-pool
                // Registry can exhaust `max_connections` (a "Connection reset by
                // peer"), which is itself a measured finding — we record the
                // failure rate rather than crashing the whole sweep.
                match metering.ingest_at(&r, period).await {
                    Ok(_) => lats.push(op.elapsed()),
                    Err(_) => errs += 1,
                }
            }
            (lats, errs)
        });
        tasks.push(task);
    }

    let mut all_lats: Vec<Duration> = Vec::new();
    let mut errors = 0usize;
    for task in tasks {
        // compio `Task::await` yields `Result<T, _>` (a join result).
        let (lats, errs) = task.await.expect("join ingest task");
        all_lats.extend(lats);
        errors += errs;
    }
    let wall = started.elapsed();
    let total = all_lats.len();
    let rps = total as f64 / wall.as_secs_f64();
    let (p50, p95, p99, mean) = percentiles(all_lats);
    let err_note = if errors > 0 {
        format!("  ({errors} conn-ceiling errs)")
    } else {
        String::new()
    };
    println!(
        "{:<10} {:>6} {:>6} {:>9} {:>11.0} {:>9.2} {:>9.2} {:>9.2} {:>9.2}{}",
        sc.label, sc.apps, sc.concurrency, total, rps, p50, p95, p99, mean, err_note
    );

    // Let the detached connection-driver tasks from this scenario's per-call
    // connections tear down (free their PG backends) before the next scenario —
    // the no-pool churn otherwise stacks dead-but-not-yet-freed backends toward
    // the `max_connections` ceiling.
    compio::time::sleep(Duration::from_millis(1500)).await;
}

// ===========================================================================
// Bench 2 — spend-enforcement fleet sweep (SpendEngine::evaluate_all).
// ===========================================================================

async fn bench_spend(registry: &Registry, admin: &Client) {
    println!("\n================================================================");
    println!("BENCH 2 — spend fleet sweep (REAL SpendEngine::evaluate_all)");
    println!("================================================================");
    println!(
        "Seeds N apps each with one usage_aggregates row in the CURRENT period,\n\
         then times the real evaluate_all sweep (bulk-prefilter + per-app derive).\n\
         NOTE: evaluate_all reads `apps` fleet-wide, so it also sees apps left by\n\
         earlier benches — the N below is the count THIS bench added."
    );
    println!("\n{:>8} {:>12} {:>14} {:>16}", "apps", "sweep_ms", "apps/sec", "ms_per_app");

    for &n in &[100usize, 1_000, 5_000] {
        // Seed N apps with a current-period usage row each (bulk insert).
        let app_ids = seed_apps_with_current_usage(admin, n).await;

        let engine = SpendEngine::new(registry.clone());
        let started = Instant::now();
        let transitions = engine.evaluate_all().await.expect("evaluate_all");
        let wall = started.elapsed();
        let ms = wall.as_secs_f64() * 1000.0;
        // evaluate_all sees the whole fleet; report against N (this bench's adds)
        // plus the observed transition count for context.
        let per_app = ms / n as f64;
        println!(
            "{:>8} {:>12.1} {:>14.0} {:>16.4}   (+{} transitions, fleet-wide)",
            n,
            ms,
            n as f64 / wall.as_secs_f64(),
            per_app,
            transitions.len()
        );

        // Clean THIS batch's apps so the next N is measured against a fleet that
        // grew only by the earlier benches, not by N compounding.
        cleanup_apps(admin, &app_ids).await;
    }
}

/// Bulk-seed N apps, each with a single `requests` usage_aggregates row in the
/// CURRENT calendar-month period (what evaluate_all prices).
async fn seed_apps_with_current_usage(admin: &Client, n: usize) -> Vec<Uuid> {
    let mut ids = Vec::with_capacity(n);
    // Insert apps in chunks of multi-row VALUES to keep the seed fast.
    let mut i = 0;
    while i < n {
        let chunk = (n - i).min(500);
        let mut sql = String::from(
            "INSERT INTO zeroship.apps (name, plan_id, api_key, api_key_hash) VALUES ",
        );
        let mut params: Vec<Box<dyn compio_postgres::types::ToSql + Sync>> = Vec::new();
        for j in 0..chunk {
            if j > 0 {
                sql.push(',');
            }
            let p = j * 2;
            sql.push_str(&format!("(${}, 'pln_metering_load', ${}, '')", p + 1, p + 2));
            params.push(Box::new(format!("ml-spend-{}-{}", i + j, Uuid::new_v4())));
            params.push(Box::new(Uuid::new_v4().to_string()));
        }
        sql.push_str(" RETURNING id");
        let param_refs: Vec<&(dyn compio_postgres::types::ToSql + Sync)> =
            params.iter().map(|b| b.as_ref()).collect();
        let rows = admin.query(&sql, &param_refs).await.expect("bulk insert apps");
        for r in &rows {
            ids.push(r.get::<_, Uuid>("id"));
        }
        i += chunk;
    }

    // One usage row per app in the current period (a moderate `requests` total
    // so pricing is non-trivial but under the 100000c default cap → mostly
    // `allow`/`warn`, exercising the derive + touch_state write per app).
    let period = period_date(period_start_unix(chrono::Utc::now().timestamp()));
    let mut i = 0;
    while i < ids.len() {
        let chunk = (ids.len() - i).min(500);
        let mut sql = String::from(
            "INSERT INTO zeroship.usage_aggregates (app_id, period, metric, total, updated_at) VALUES ",
        );
        let mut params: Vec<Box<dyn compio_postgres::types::ToSql + Sync>> = Vec::new();
        for j in 0..chunk {
            if j > 0 {
                sql.push(',');
            }
            let p = j;
            sql.push_str(&format!("(${}, ${}::date, 'requests', 5000, NOW())", p + 1, chunk + 1));
            params.push(Box::new(ids[i + j]));
        }
        params.push(Box::new(period));
        let param_refs: Vec<&(dyn compio_postgres::types::ToSql + Sync)> =
            params.iter().map(|b| b.as_ref()).collect();
        admin.query(&sql, &param_refs).await.expect("bulk insert usage");
        i += chunk;
    }
    ids
}

async fn cleanup_apps(admin: &Client, ids: &[Uuid]) {
    // Delete dependent rows first (FK order), then the apps. Chunked to keep
    // parameter counts sane.
    for chunk in ids.chunks(1000) {
        admin
            .execute(
                "DELETE FROM zeroship.usage_aggregates WHERE app_id = ANY($1)",
                &[&chunk],
            )
            .await
            .ok();
        admin
            .execute(
                "DELETE FROM zeroship.app_spend_state WHERE app_id = ANY($1)",
                &[&chunk],
            )
            .await
            .ok();
        admin
            .execute(
                "DELETE FROM zeroship.app_spend_limit WHERE app_id = ANY($1)",
                &[&chunk],
            )
            .await
            .ok();
        admin
            .execute("DELETE FROM zeroship.apps WHERE id = ANY($1)", &[&chunk])
            .await
            .ok();
    }
}

// ===========================================================================
// Bench 3 — reconcile read pattern at scale (O(active) vs O(all-apps-ever)).
// ===========================================================================

async fn bench_reconcile(db_url: &str, admin: &Client) {
    println!("\n================================================================");
    println!("BENCH 3 — reconcile read pattern (O(active) vs O(all-apps-ever))");
    println!("================================================================");
    println!(
        "Replays the REAL billing_reconcile::sweep READ shape over a CLOSED period:\n\
         the owner-grouping query + the active-app usage_aggregates prefilter, then\n\
         per-ACTIVE-app Metering::period_totals_on + the plan lookup. Stripe POSTs\n\
         + invoice writes are out of scope (a fixed per-active-creator network cost).\n\
         Holds total apps fixed and varies the ACTIVE fraction to show the cost\n\
         tracks active apps, not all-apps-ever."
    );
    println!(
        "\n{:>10} {:>10} {:>10} {:>12} {:>16}",
        "total_apps", "active", "owners_ms", "reads_ms", "ms_per_active"
    );

    let closed_period =
        zeroship_control::cron::billing_reconcile::previous_period_start_unix(
            chrono::Utc::now().timestamp(),
        );

    // Fixed total fleet, three active fractions: if the cost is O(active) the
    // reads_ms should scale with `active`, NOT with `total_apps`.
    let total_apps = 4_000usize;
    for &active in &[100usize, 1_000, 4_000] {
        run_reconcile_scenario(db_url, admin, total_apps, active, closed_period).await;
    }
}

async fn run_reconcile_scenario(
    db_url: &str,
    admin: &Client,
    total_apps: usize,
    active: usize,
    closed_period: i64,
) {
    // One creator per app (worst case for owner-grouping fan-out: every app a
    // distinct owner). Seed `total_apps` owned apps; the first `active` of them
    // also get a usage_aggregates row in the CLOSED period.
    let (creators, app_ids) = seed_owned_apps(admin, total_apps).await;
    seed_closed_usage(admin, &app_ids[..active], closed_period).await;

    // A FRESH dedicated connection for the reconcile reads — faithful to
    // `billing_reconcile::sweep`, which opens one `registry.conn()` and runs the
    // owner-grouping + active-prefilter + per-app reads on it. `Registry::conn`
    // is `pub(crate)`, so the bench opens an equivalent `Client` directly.
    let conn = connect(db_url).await;

    // --- The owner-grouping + active-app prefilter (verbatim from sweep). ---
    let owners_started = Instant::now();
    let owner_rows = conn
        .query(
            "SELECT DISTINCT ON (m.app_id) m.user_id AS creator_id, m.app_id \
             FROM zeroship.app_members m \
             WHERE m.role = 'owner' \
             ORDER BY m.app_id, m.user_id",
            &[],
        )
        .await
        .expect("owner rows");
    let period = period_date(closed_period);
    let active_rows = conn
        .query(
            "SELECT DISTINCT app_id FROM zeroship.usage_aggregates WHERE period = $1::date",
            &[&period],
        )
        .await
        .expect("active rows");
    let active_set: std::collections::HashSet<Uuid> =
        active_rows.iter().map(|r| r.get::<_, Uuid>("app_id")).collect();
    let mut apps_by_creator: std::collections::BTreeMap<Uuid, Vec<Uuid>> =
        std::collections::BTreeMap::new();
    for row in &owner_rows {
        let app_id: Uuid = row.get("app_id");
        if !active_set.contains(&app_id) {
            continue;
        }
        let creator_id: Uuid = row.get("creator_id");
        apps_by_creator.entry(creator_id).or_default().push(app_id);
    }
    let owners_ms = owners_started.elapsed().as_secs_f64() * 1000.0;

    // --- Per-ACTIVE-app reads: the real period_totals_on + plan lookup. ---
    let reads_started = Instant::now();
    let mut priced = 0usize;
    for app_ids in apps_by_creator.values() {
        for app_id in app_ids {
            // Real per-app metering read (the reconcile path's source of truth).
            let totals = Metering::period_totals_on(&conn, app_id, closed_period)
                .await
                .expect("period_totals_on");
            // The plan lookup — body of the pub(crate) lookup_plan_id_on, inlined
            // (it is not callable from a bench crate).
            let _plan = conn
                .query("SELECT plan_id FROM zeroship.apps WHERE id = $1", &[app_id])
                .await
                .expect("plan lookup");
            if !totals.is_empty() {
                priced += 1;
            }
        }
    }
    let reads_ms = reads_started.elapsed().as_secs_f64() * 1000.0;
    let per_active = if priced == 0 { 0.0 } else { reads_ms / priced as f64 };

    println!(
        "{:>10} {:>10} {:>10.2} {:>12.1} {:>16.4}",
        total_apps, active, owners_ms, reads_ms, per_active
    );

    // Self-clean this scenario's seed.
    let creator_refs: Vec<Uuid> = creators;
    cleanup_owned(admin, &app_ids, &creator_refs).await;
}

/// Seed `n` owned apps (one distinct creator each). Returns (creators, app_ids).
async fn seed_owned_apps(admin: &Client, n: usize) -> (Vec<Uuid>, Vec<Uuid>) {
    let mut creators = Vec::with_capacity(n);
    let mut apps = Vec::with_capacity(n);

    // Bulk users.
    let mut i = 0;
    while i < n {
        let chunk = (n - i).min(500);
        let mut sql = String::from("INSERT INTO zeroship.users (email, name) VALUES ");
        let mut params: Vec<Box<dyn compio_postgres::types::ToSql + Sync>> = Vec::new();
        for j in 0..chunk {
            if j > 0 {
                sql.push(',');
            }
            let p = j * 2;
            sql.push_str(&format!("(${}, ${})", p + 1, p + 2));
            params.push(Box::new(format!("ml-cr-{}-{}@ex.test", i + j, Uuid::new_v4().simple())));
            params.push(Box::new("ml".to_string()));
        }
        sql.push_str(" RETURNING id");
        let pr: Vec<&(dyn compio_postgres::types::ToSql + Sync)> =
            params.iter().map(|b| b.as_ref()).collect();
        let rows = admin.query(&sql, &pr).await.expect("bulk users");
        for r in &rows {
            creators.push(r.get::<_, Uuid>("id"));
        }
        i += chunk;
    }

    // Bulk apps.
    i = 0;
    while i < n {
        let chunk = (n - i).min(500);
        let mut sql = String::from(
            "INSERT INTO zeroship.apps (name, plan_id, api_key, api_key_hash) VALUES ",
        );
        let mut params: Vec<Box<dyn compio_postgres::types::ToSql + Sync>> = Vec::new();
        for j in 0..chunk {
            if j > 0 {
                sql.push(',');
            }
            let p = j * 2;
            sql.push_str(&format!("(${}, 'pln_metering_load', ${}, '')", p + 1, p + 2));
            params.push(Box::new(format!("ml-rapp-{}-{}", i + j, Uuid::new_v4())));
            params.push(Box::new(Uuid::new_v4().to_string()));
        }
        sql.push_str(" RETURNING id");
        let pr: Vec<&(dyn compio_postgres::types::ToSql + Sync)> =
            params.iter().map(|b| b.as_ref()).collect();
        let rows = admin.query(&sql, &pr).await.expect("bulk apps");
        for r in &rows {
            apps.push(r.get::<_, Uuid>("id"));
        }
        i += chunk;
    }

    // Bulk owner memberships (creator[k] owns app[k]).
    i = 0;
    while i < n {
        let chunk = (n - i).min(500);
        let mut sql = String::from(
            "INSERT INTO zeroship.app_members (app_id, user_id, role) VALUES ",
        );
        let mut params: Vec<Box<dyn compio_postgres::types::ToSql + Sync>> = Vec::new();
        for j in 0..chunk {
            if j > 0 {
                sql.push(',');
            }
            let p = j * 2;
            sql.push_str(&format!("(${}, ${}, 'owner')", p + 1, p + 2));
            params.push(Box::new(apps[i + j]));
            params.push(Box::new(creators[i + j]));
        }
        let pr: Vec<&(dyn compio_postgres::types::ToSql + Sync)> =
            params.iter().map(|b| b.as_ref()).collect();
        admin.execute(&sql, &pr).await.expect("bulk members");
        i += chunk;
    }

    (creators, apps)
}

/// Seed a CLOSED-period usage row for each app in `apps`.
async fn seed_closed_usage(admin: &Client, apps: &[Uuid], closed_period: i64) {
    let period = period_date(closed_period);
    let mut i = 0;
    while i < apps.len() {
        let chunk = (apps.len() - i).min(500);
        let mut sql = String::from(
            "INSERT INTO zeroship.usage_aggregates (app_id, period, metric, total, updated_at) VALUES ",
        );
        let mut params: Vec<Box<dyn compio_postgres::types::ToSql + Sync>> = Vec::new();
        for j in 0..chunk {
            if j > 0 {
                sql.push(',');
            }
            sql.push_str(&format!("(${}, ${}::date, 'requests', 1234, NOW())", j + 1, chunk + 1));
            params.push(Box::new(apps[i + j]));
        }
        params.push(Box::new(period));
        let pr: Vec<&(dyn compio_postgres::types::ToSql + Sync)> =
            params.iter().map(|b| b.as_ref()).collect();
        admin.query(&sql, &pr).await.expect("bulk closed usage");
        i += chunk;
    }
}

async fn cleanup_owned(admin: &Client, apps: &[Uuid], creators: &[Uuid]) {
    for chunk in apps.chunks(1000) {
        admin
            .execute("DELETE FROM zeroship.usage_aggregates WHERE app_id = ANY($1)", &[&chunk])
            .await
            .ok();
        admin
            .execute("DELETE FROM zeroship.app_members WHERE app_id = ANY($1)", &[&chunk])
            .await
            .ok();
        admin
            .execute("DELETE FROM zeroship.apps WHERE id = ANY($1)", &[&chunk])
            .await
            .ok();
    }
    for chunk in creators.chunks(1000) {
        admin
            .execute("DELETE FROM zeroship.users WHERE id = ANY($1)", &[&chunk])
            .await
            .ok();
    }
}
