//! FAITHFUL live e2e for the OpenMeter metering-export provider (M-OpenMeter)
//! against a REAL, locally-running OpenMeter — NOT the in-test mock.
//!
//! This is the [[feedback_faithful_e2e_tests]] capstone for the OpenMeter rail:
//! it drives the SAME hardened `metering_export` cron through the REAL
//! `OpenMeterProvider`/`OpenMeterClient` (cyper over the wire) against a live
//! OpenMeter stack (CloudEvents ingest → Kafka → sink-worker → ClickHouse →
//! `/query` aggregate). It catches the real-API divergences the mock cannot,
//! because the mock:
//!   * ANSWERS the aggregate SYNCHRONOUSLY (real OpenMeter is eventually
//!     consistent — the sink-worker drains Kafka into ClickHouse async), and
//!   * IGNORES the `/query` `from`/`to` window (it sums by subject regardless),
//!     whereas REAL OpenMeter filters the aggregate to events whose `time` falls
//!     inside `[from, to)`. The provider's `reported_total` queries
//!     `[period.start, period.end)` and `report_usage` stamps `time` at `now`, so
//!     the reconcile only works when `now ∈ [period.start, period.end)` — i.e.
//!     the period must be the CURRENT calendar month (NOT a far-future synthetic
//!     bucket as the mock test uses). See docs/reference/billing-metering.md
//!     (OpenMeter §"Real-API divergences from the mock").
//!
//! GATING: requires BOTH a real Postgres (`CONTROL_TEST_DB`, changeset 0043
//! applied) AND a live OpenMeter base URL (`OPENMETER_LIVE_URL`, e.g.
//! `http://127.0.0.1:48888`). Marked `#[ignore]` so it never runs in the default
//! `cargo test` sweep — `tests/e2e_openmeter_export.sh` owns the compose + PG
//! lifecycle and invokes it with `--ignored`. If either env var is unset the
//! test prints a skip note and returns green.

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use uuid::Uuid;

use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::cron::metering_export;
use zeroship_control::metering::provider::{
    build_provider, MeteringProviderConfig, OpenMeterConfig,
};
use zeroship_control::metering::{current_period_start_unix, Metering};
use zeroship_control::openmeter_client::{OpenMeterApi, OpenMeterClient};
use zeroship_control::{
    AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_core::types::{AppUsage, UsageReport};

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const OM_EVENT_TYPE: &str = "compute_units";
const OM_METER_SLUG: &str = "compute_units";

/// Like the mock suite: the export sweep single-flights fleet-wide via a
/// `pg_try_advisory_lock`; serialize the sweep-driving tests so the `n == 1`
/// assertions hold (poison-recovered).
static EXPORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

fn openmeter_url() -> Option<String> {
    std::env::var("OPENMETER_LIVE_URL").ok()
}

/// The token the live stack accepts. The quickstart config has no auth gate, so
/// any Bearer token is accepted; allow an override for a token-gated deployment.
fn openmeter_token() -> String {
    std::env::var("OPENMETER_LIVE_TOKEN").unwrap_or_else(|_| "om_live_e2e".to_string())
}

fn tmpdir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("zs-omlive-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&p).expect("mk tmpdir");
    p
}

struct Fixture {
    state: Arc<AppState>,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
    om_base: String,
    om_token: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

async fn build_fixture(db_url: &str, om_base: &str, label: &str) -> Fixture {
    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("dtmp-{label}"));
    let registry = Registry::new(db_url).await.expect("registry");
    zeroship_control::bootstrap_console::seed_plans(&registry)
        .await
        .expect("seed built-in plans");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY, false).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));

    let (control_pg_client, control_pg_conn) =
        compio_postgres::connect(db_url, compio_postgres::NoTls)
            .await
            .expect("control-pg connect");
    compio::runtime::spawn(async move {
        let _ = control_pg_conn.run().await;
    })
    .detach();
    let control_pg = Arc::new(control_pg_client);

    let om_token = openmeter_token();
    // THE faithful seam: a REAL OpenMeterProvider whose base URL is the LIVE
    // OpenMeter. Every export pushes a CloudEvent through the real cyper client
    // over the wire and reads the real ClickHouse-backed aggregate back.
    let provider = build_provider(&MeteringProviderConfig::openmeter(OpenMeterConfig {
        base_url: om_base.to_string(),
        token: SecretString::new(om_token.clone()),
        event_type: OM_EVENT_TYPE.to_string(),
        meter_slug: OM_METER_SLUG.to_string(),
    }))
    .expect("openmeter provider builds");

    let state = Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        blob_store,
        control_key: SecretString::new("test-control-key".to_string()),
        master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
        stripe_webhook_secret: SecretString::new(String::new()),
        stripe_secret_key: SecretString::new("sk_test_mock".to_string()),
        stripe_base_url: "http://127.0.0.1:9".to_string(),
        worker_urls: Vec::new(),
        worker_key: SecretString::new(String::new()),
        admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        insecure_dev: false,
        trust_proxy: false,
        deploy_tmp_dir: deploy_tmp_dir.clone(),
        control_pg,
        hydra_admin_url: "http://127.0.0.1:9".to_string(),
        app_base_domain: "zeroship.localhost".to_string(),
        trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
        expected_oauth_audience: "control.zeroship.ai".to_string(),
        static_policies: zeroship_authz::load_platform_policies()
            .expect("bundled authz policies parse"),
        pat_issuer: Arc::new(zeroship_authn::PatIssuer::dev_insecure()),
        auth_provider: zeroship_control::hydra_auth_provider("http://127.0.0.1:9"),
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        metering_provider: provider,
        tax_provider: zeroship_control::tax::build_tax_provider(
            &zeroship_control::tax::TaxProviderConfig::native(),
        )
        .expect("native tax provider builds"),
        notifier: std::sync::Arc::new(zeroship_control::notify::RecordingNotifier::new()),
        pairwise_salt: [0u8; 32],
        projected_charge_cache: std::sync::Arc::new(
            zeroship_control::billing_read::ProjectedChargeCache::default(),
        ),
    });

    Fixture {
        state,
        blob_root,
        deploy_tmp_dir,
        om_base: om_base.to_string(),
        om_token,
    }
}

// --- DB seeding helpers (mirror the mock suite) ----------------------------

async fn make_user(state: &AppState, label: &str) -> Uuid {
    let email = format!("{label}-{}@example.test", Uuid::new_v4().simple());
    let rows = state
        .control_pg
        .query(
            "INSERT INTO zeroship.users (email, name) VALUES ($1, $2) RETURNING id",
            &[&email, &"Live Creator".to_string()],
        )
        .await
        .expect("insert user");
    rows[0].get("id")
}

async fn make_plan_with_included(state: &AppState, included_units: i64) -> String {
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units) \
             VALUES ('requests', 1, 1) \
             ON CONFLICT (metric) DO UPDATE SET units_per_op = 1, per_units = 1",
            &[],
        )
        .await
        .expect("upsert requests weight");
    let plan_id = format!("pln_omlive_{}", Uuid::new_v4().simple());
    let fx_one_cent: i64 = 1_000_000_000_000;
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'omlive-test', 0, $2, $3, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', 100000)",
            &[&plan_id, &included_units, &fx_one_cent],
        )
        .await
        .expect("seed priced plan");
    plan_id
}

async fn make_plan(state: &AppState) -> String {
    make_plan_with_included(state, 0).await
}

async fn make_owned_app(state: &AppState, plan_id: &str, owner: Uuid) -> Uuid {
    let name = format!("omlive-{}", Uuid::new_v4());
    let rows = state
        .control_pg
        .query(
            "INSERT INTO zeroship.apps (name, plan_id, api_key, api_key_hash) \
             VALUES ($1, $2, $3, '') RETURNING id",
            &[&name, &plan_id, &Uuid::new_v4().to_string()],
        )
        .await
        .expect("insert app");
    let app_id: Uuid = rows[0].get("id");
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.app_members (app_id, user_id, role) VALUES ($1, $2, 'owner')",
            &[&app_id, &owner],
        )
        .await
        .expect("insert owner membership");
    app_id
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

async fn ingest_at(state: &AppState, app: Uuid, requests: u64, period_start: i64, seq: u64) {
    let metering = Metering::new(state.registry.clone());
    let worker = format!("w-{}", Uuid::new_v4());
    metering
        .ingest_at(&report(&worker, seq, app, requests), period_start)
        .await
        .expect("ingest usage");
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Poll the LIVE OpenMeter aggregate for `subject` over the current period until
/// it reaches `want` (the eventual-consistency wait the mock never needs).
/// Returns the observed value (which may be `< want` on timeout, so the caller
/// can assert with a helpful message). Uses the REAL cyper `OpenMeterClient`.
async fn poll_aggregate(fx: &Fixture, subject: &str, period_start: i64, want: u64) -> u64 {
    let client = OpenMeterClient::new(SecretString::new(fx.om_token.clone()))
        .with_base_url(fx.om_base.clone());
    let period_end = zeroship_control::cron::billing_reconcile::period_end_unix(period_start);
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last = 0u64;
    loop {
        last = client
            .meter_query(OM_METER_SLUG, subject, period_start, period_end)
            .await
            .unwrap_or(last);
        if last >= want || Instant::now() >= deadline {
            return last;
        }
        compio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ===========================================================================
// Test 1 — CloudEvents ACCEPTED + aggregate reconciled (the core faithful path).
// ===========================================================================

/// The export cron pushes a CloudEvent through the REAL client to LIVE OpenMeter;
/// OpenMeter ACCEPTS it (204), the sink-worker lands it in ClickHouse, and the
/// real `/query` aggregate (polled for convergence) returns the pushed CU — the
/// SAME value `reported_total` reads back. Proves (a) accept + (b) reconcile.
#[compio::test]
#[ignore = "requires a live OpenMeter (OPENMETER_LIVE_URL) + PG (CONTROL_TEST_DB); run via tests/e2e_openmeter_export.sh"]
async fn live_export_pushes_cu_and_aggregate_reconciles() {
    let (Some(url), Some(om)) = (db_url(), openmeter_url()) else {
        eprintln!("skip: CONTROL_TEST_DB and/or OPENMETER_LIVE_URL not set");
        return;
    };
    let fx = build_fixture(&url, &om, "push").await;
    let _export = EXPORT_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // CURRENT month — so `now` falls inside [period.start, period.end), which is
    // required for the live aggregate query window to contain the event (the
    // divergence from the mock, which uses far-future buckets).
    let period = current_period_start_unix();

    let creator = make_user(&fx.state, "push").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    // A UNIQUE subject per run so re-runs in the same month don't collide on
    // OpenMeter's per-subject aggregate.
    let cus = format!("cus_omlive_push_{}", Uuid::new_v4().simple());
    fx.state.stripe_store.set_customer(creator, &cus).await.unwrap();

    ingest_at(&fx.state, app, 750, period, 1).await; // 750 requests × 1 CU = 750 CU

    let n = metering_export::tick_at(&fx.state, period, now_unix())
        .await
        .expect("tick");
    assert_eq!(n, 1, "one app exported to LIVE OpenMeter");

    // High-water advanced locally (the push returned 2xx → the cron committed it).
    let hw = read_high_water(&fx.state, &creator, period).await;
    assert_eq!(hw, Some(750), "exported_units high-water == cumulative CU after a 2xx ingest");

    // LIVE aggregate converges to 750 (eventual consistency: kafka→sink→clickhouse).
    let agg = poll_aggregate(&fx, &cus, period, 750).await;
    assert_eq!(
        agg, 750,
        "LIVE OpenMeter aggregate == 750 CU for the subject/period (CloudEvents accepted + reconciled)"
    );
}

// ===========================================================================
// Test 2 — delta export across two ticks, reconciled via the LIVE aggregate.
// ===========================================================================

/// Two ticks: tick 1 exports N, usage grows to M, tick 2 exports the DELTA M−N
/// (driven by `reported_total` reading the LIVE aggregate), and OpenMeter SUMS to
/// M. Proves the C2 delta math feeds off the REAL aggregate, exactly-once.
#[compio::test]
#[ignore = "requires a live OpenMeter (OPENMETER_LIVE_URL) + PG (CONTROL_TEST_DB); run via tests/e2e_openmeter_export.sh"]
async fn live_export_computes_delta_via_real_aggregate() {
    let (Some(url), Some(om)) = (db_url(), openmeter_url()) else {
        eprintln!("skip: CONTROL_TEST_DB and/or OPENMETER_LIVE_URL not set");
        return;
    };
    let fx = build_fixture(&url, &om, "delta").await;
    let _export = EXPORT_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let period = current_period_start_unix();

    let creator = make_user(&fx.state, "delta").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    let cus = format!("cus_omlive_delta_{}", Uuid::new_v4().simple());
    fx.state.stripe_store.set_customer(creator, &cus).await.unwrap();

    ingest_at(&fx.state, app, 100, period, 1).await;
    let n1 = metering_export::tick_at(&fx.state, period, now_unix()).await.expect("tick 1");
    assert_eq!(n1, 1);
    // Let the first delta (100) land in the aggregate before driving tick 2 — the
    // delta math reads `reported_total` (the LIVE aggregate), so it must reflect
    // tick 1 first (otherwise tick 2 would re-push 100, then OpenMeter SUMs the
    // dedup-DISTINCT events to 350). This poll is the eventual-consistency guard.
    let after1 = poll_aggregate(&fx, &cus, period, 100).await;
    assert_eq!(after1, 100, "aggregate reflects tick 1 (100) before tick 2");

    ingest_at(&fx.state, app, 150, period, 2).await; // grows to 250

    let n2 = metering_export::tick_at(&fx.state, period, now_unix()).await.expect("tick 2");
    assert_eq!(n2, 1, "the second tick pushes the (non-zero) delta");

    let after2 = poll_aggregate(&fx, &cus, period, 250).await;
    assert_eq!(
        after2, 250,
        "LIVE aggregate SUM == cumulative 250 (tick 2 pushed the DELTA 150, not 250)"
    );
    assert_eq!(read_high_water(&fx.state, &creator, period).await, Some(250));
}

// ===========================================================================
// Test 3 — M1: BILLABLE CU (gross − included) flows through to LIVE OpenMeter.
// ===========================================================================

/// For a plan with non-zero `included_units`, the export pushes BILLABLE CU
/// (`gross − included`) — the SAME quantity the spend cap treats as billable —
/// and the LIVE aggregate reflects exactly that, not the gross total.
#[compio::test]
#[ignore = "requires a live OpenMeter (OPENMETER_LIVE_URL) + PG (CONTROL_TEST_DB); run via tests/e2e_openmeter_export.sh"]
async fn live_export_pushes_billable_cu_honoring_included_units() {
    let (Some(url), Some(om)) = (db_url(), openmeter_url()) else {
        eprintln!("skip: CONTROL_TEST_DB and/or OPENMETER_LIVE_URL not set");
        return;
    };
    let fx = build_fixture(&url, &om, "m1").await;
    let _export = EXPORT_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let period = current_period_start_unix();

    let creator = make_user(&fx.state, "m1").await;
    let included: i64 = 200; // gross 750 ⇒ billable 550
    let plan = make_plan_with_included(&fx.state, included).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    let cus = format!("cus_omlive_m1_{}", Uuid::new_v4().simple());
    fx.state.stripe_store.set_customer(creator, &cus).await.unwrap();

    ingest_at(&fx.state, app, 750, period, 1).await;

    let n = metering_export::tick_at(&fx.state, period, now_unix()).await.expect("tick");
    assert_eq!(n, 1);

    let agg = poll_aggregate(&fx, &cus, period, 550).await;
    assert_eq!(
        agg, 550,
        "LIVE OpenMeter counted the BILLABLE 550 CU (gross 750 − included 200), not gross"
    );
    assert_eq!(read_high_water(&fx.state, &creator, period).await, Some(550));
}

// --- read helpers ----------------------------------------------------------

fn period_d(period_start: i64) -> chrono::NaiveDate {
    use chrono::{Datelike, TimeZone};
    let dt = chrono::Utc.timestamp_opt(period_start, 0).single().unwrap();
    chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), 1).unwrap()
}

async fn read_high_water(state: &AppState, creator: &Uuid, period: i64) -> Option<i64> {
    state
        .control_pg
        .query(
            "SELECT exported_units FROM zeroship.metering_exports \
             WHERE creator_id = $1 AND period = $2::date",
            &[creator, &period_d(period)],
        )
        .await
        .expect("read high-water")
        .first()
        .map(|r| r.get::<_, i64>("exported_units"))
}
