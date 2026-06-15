//! FAITHFUL live e2e for the Stripe Billing Meters metering-export provider
//! (M-Stripe) against REAL `https://api.stripe.com` (test mode) — NOT the
//! in-test localhost mock (`metering_export_test.rs`).
//!
//! This is the [[feedback_faithful_e2e_tests]] capstone for the Stripe rail, the
//! exact peer of `metering_export_openmeter_live_test.rs`: it drives the SAME
//! hardened `metering_export` cron through the REAL `StripeProvider`/
//! `StripeClient` (cyper over the wire) against a REAL operator-provisioned Stripe
//! Billing Meter:
//!
//!   ingest_at (usage) ─► metering_export::tick_at
//!        │  report_usage → POST /v1/billing/meter_events       ─► api.stripe.com
//!        │                                                         │ (async agg)
//!        │  reported_total → GET /v1/billing/meters/<id>/         │
//!        │                       event_summaries  ◄── Stripe meter aggregate
//!        ▼
//!   exported_units high-water (Postgres)
//!
//! FAITHFUL by construction — NOTHING under test is stubbed:
//!   * REAL Stripe Billing Meters (meter_events ingest → Stripe's async
//!     aggregation → event_summaries readback), provisioned by the harness via
//!     `POST /v1/billing/meters` and passed in as `STRIPE_LIVE_METER_ID`.
//!   * REAL zeroship `StripeProvider`/`StripeClient` (cyper) pointed at
//!     `https://api.stripe.com` with the operator's TEST secret key.
//!   * REAL ephemeral zeroship Postgres + the full Liquibase changelog (the cron
//!     reads/writes usage_aggregates + metering_exports).
//!
//! ## Real-API divergences from the mock (the whole point — what the mock hid)
//!
//! The localhost mock (`metering_export_test.rs`):
//!   * ANSWERS the `event_summaries` aggregate SYNCHRONOUSLY (real Stripe
//!     aggregates meter events ASYNC — the readback lags the push by seconds to
//!     minutes), so the live test MUST POLL for convergence (`poll_aggregate`);
//!   * IGNORES the summary `start_time`/`end_time` window (it sums by customer
//!     regardless), whereas REAL Stripe filters the aggregate to events whose
//!     `timestamp` falls inside `[start_time, end_time)` AND, with
//!     `value_grouping_window=day` (which the provider sends), REQUIRES both
//!     bounds to be UTC-day-aligned. The provider's `reported_total` queries
//!     `[period.start, period.end)` and `report_usage` stamps the event at `now`,
//!     so the reconcile ONLY works when `now ∈ [period.start, period.end)` — i.e.
//!     the period must be the CURRENT calendar month (NOT a far-future synthetic
//!     bucket as the mock test uses). This is the SAME timestamp-window bug-class
//!     as C1 (a `period.end`-stamped event would be a future timestamp Stripe
//!     rejects) and the OpenMeter window divergence.
//!   * NEVER enforces the real `[now−35d, now+5min]` meter-event timestamp window
//!     except via a hand-rolled re-implementation; here REAL Stripe enforces it.
//!
//! GATING: requires BOTH a real Postgres (`CONTROL_TEST_DB`, changeset 0043
//! applied) AND a REAL Stripe TEST secret key (`STRIPE_LIVE_SECRET_KEY`,
//! `sk_test_…`) AND a REAL provisioned meter id (`STRIPE_LIVE_METER_ID`,
//! `mtr_…`). Marked `#[ignore]` so it NEVER runs in the default `cargo test`
//! sweep — `tests/e2e_stripe_meters_export.sh` owns the DB + meter lifecycle and
//! invokes it with `--ignored`. If any env var is unset the test prints a skip
//! note and returns green. The event_name the provider posts is configurable via
//! `STRIPE_LIVE_METER_EVENT_NAME` (default `compute_units`), matching the meter
//! the harness creates.
//!
//! SECRETS: the secret key is read from `STRIPE_LIVE_SECRET_KEY` ONLY — never
//! hardcoded, never logged (the provider's Debug redacts it; this test never
//! prints it).

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use uuid::Uuid;

use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::cron::billing_reconcile::period_end_unix;
use zeroship_control::cron::metering_export;
use zeroship_control::metering::provider::{
    build_provider, MeteringProviderConfig, StripeMeterConfig,
};
use zeroship_control::metering::{current_period_start_unix, Metering};
use zeroship_control::stripe_client::{StripeApi, StripeClient};
use zeroship_control::{
    AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_core::types::{AppUsage, UsageReport};

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const STRIPE_LIVE_BASE_URL: &str = "https://api.stripe.com";

/// Like the mock + OpenMeter live suites: the export sweep single-flights
/// fleet-wide via a `pg_try_advisory_lock`; serialize the sweep-driving tests so
/// the `n == 1` assertions hold (poison-recovered).
static EXPORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

/// The REAL Stripe TEST secret key (`sk_test_…`). Read from the env ONLY.
fn stripe_secret_key() -> Option<String> {
    std::env::var("STRIPE_LIVE_SECRET_KEY").ok().filter(|k| !k.trim().is_empty())
}

/// The REAL operator-provisioned meter id (`mtr_…`) the harness created.
fn stripe_meter_id() -> Option<String> {
    std::env::var("STRIPE_LIVE_METER_ID").ok().filter(|k| !k.trim().is_empty())
}

/// The meter's `event_name` (must match the meter the harness created). Default
/// `compute_units` — the same name the mock suite + the docs example use.
fn stripe_event_name() -> String {
    std::env::var("STRIPE_LIVE_METER_EVENT_NAME")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .unwrap_or_else(|| "compute_units".to_string())
}

/// All three required env vars, or `None` (→ skip).
fn live_env() -> Option<(String, String, String)> {
    Some((db_url()?, stripe_secret_key()?, stripe_meter_id()?))
}

fn tmpdir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("zs-stripelive-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&p).expect("mk tmpdir");
    p
}

struct Fixture {
    state: Arc<AppState>,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
    secret_key: String,
    meter_id: String,
    event_name: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

async fn build_fixture(db_url: &str, secret_key: &str, meter_id: &str, label: &str) -> Fixture {
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

    let event_name = stripe_event_name();
    // THE faithful seam: a REAL StripeProvider whose base URL is the LIVE Stripe
    // API. Every export pushes a meter_event through the real cyper client over
    // the wire to api.stripe.com and reads the REAL meter aggregate back.
    let provider = build_provider(&MeteringProviderConfig::stripe(StripeMeterConfig {
        event_name: event_name.clone(),
        meter_id: meter_id.to_string(),
        secret_key: SecretString::new(secret_key.to_string()),
        base_url: STRIPE_LIVE_BASE_URL.to_string(),
    }))
    .expect("stripe provider builds");

    let state = Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        blob_store,
        control_key: SecretString::new("test-control-key".to_string()),
        master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
        stripe_webhook_secret: SecretString::new(String::new()),
        // The platform Stripe key the ensure_customer path uses is the SAME real
        // test key (ensure_customer creates a real cus_… via the Native path).
        stripe_secret_key: SecretString::new(secret_key.to_string()),
        stripe_base_url: STRIPE_LIVE_BASE_URL.to_string(),
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
        pat_issuer: Arc::new(zeroship_control::token_handlers::PatIssuer::dev_insecure()),
        hydra_introspector: Arc::new(zeroship_core::hydra::HydraIntrospector::new(
            "http://127.0.0.1:9",
        )),
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
        secret_key: secret_key.to_string(),
        meter_id: meter_id.to_string(),
        event_name,
    }
}

// --- DB seeding helpers (mirror the mock + OpenMeter live suites) -----------

async fn make_user(state: &AppState, label: &str) -> Uuid {
    let email = format!("{label}-{}@example.test", Uuid::new_v4().simple());
    let rows = state
        .control_pg
        .query(
            "INSERT INTO zeroship.users (email, name) VALUES ($1, $2) RETURNING id",
            &[&email, &"Stripe-Live Creator".to_string()],
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
    let plan_id = format!("pln_stripelive_{}", Uuid::new_v4().simple());
    let fx_one_cent: i64 = 1_000_000_000_000;
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'stripelive-test', 0, $2, $3, \
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
    let name = format!("stripelive-{}", Uuid::new_v4());
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

/// Create a REAL Stripe Customer (`cus_…`) via the SAME `StripeApi::create_customer`
/// path `ensure_customer` uses, and persist the creator↔cus_ mapping the export
/// sweep resolves. The meter aggregates events by THIS customer id. A fresh
/// customer per run guarantees the per-customer aggregate starts at 0 (no
/// cross-run contamination on the shared test account).
async fn make_real_customer(fx: &Fixture, creator: Uuid, label: &str) -> String {
    let client = StripeClient::new(SecretString::new(fx.secret_key.clone()))
        .with_base_url(STRIPE_LIVE_BASE_URL.to_string());
    let email = format!("{label}-{}@zeroship.test", Uuid::new_v4().simple());
    let cus = StripeApi::create_customer(&client, &email, &creator.to_string())
        .await
        .expect("create real Stripe test customer");
    assert!(cus.starts_with("cus_"), "expected a real cus_ id, got {cus}");
    fx.state.stripe_store.set_customer(creator, &cus).await.unwrap();
    cus
}

/// Poll the REAL Stripe meter aggregate for `customer` over the current period
/// until it reaches `want` (the eventual-consistency wait the mock never needs —
/// Stripe aggregates meter events ASYNC). Returns the observed value (which may
/// be `< want` on timeout, so the caller can assert with a helpful message). Uses
/// the REAL cyper `StripeClient::meter_event_summary` — the SAME readback the
/// provider's `reported_total` (and thus the C2 reconcile) performs.
///
/// `timeout_secs` is generous (Stripe's meter aggregation can lag minutes).
async fn poll_aggregate(
    fx: &Fixture,
    customer: &str,
    period_start: i64,
    want: u64,
    timeout_secs: u64,
) -> u64 {
    let client = StripeClient::new(SecretString::new(fx.secret_key.clone()))
        .with_base_url(STRIPE_LIVE_BASE_URL.to_string());
    let period_end = period_end_unix(period_start);
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut last = 0u64;
    loop {
        last = client
            .meter_event_summary(&fx.meter_id, customer, period_start, period_end)
            .await
            .unwrap_or(last);
        if last >= want || Instant::now() >= deadline {
            return last;
        }
        compio::time::sleep(Duration::from_secs(3)).await;
    }
}

// ===========================================================================
// Test 1 — meter_event ACCEPTED by REAL Stripe + aggregate reconciles.
// ===========================================================================

/// The export cron pushes a `meter_event` through the REAL client to LIVE Stripe;
/// Stripe ACCEPTS it (2xx), aggregates it ASYNC, and the real `event_summaries`
/// aggregate (POLLED for convergence) returns the pushed CU — the SAME value
/// `reported_total` reads back. Proves (a) accept against the real timestamp
/// window + payload shape, and (b) the reconcile readback path.
#[compio::test]
#[ignore = "requires a live Stripe TEST key (STRIPE_LIVE_SECRET_KEY) + meter (STRIPE_LIVE_METER_ID) + PG (CONTROL_TEST_DB); run via tests/e2e_stripe_meters_export.sh"]
async fn live_export_pushes_cu_and_aggregate_reconciles() {
    let Some((url, sk, meter)) = live_env() else {
        eprintln!("skip: CONTROL_TEST_DB / STRIPE_LIVE_SECRET_KEY / STRIPE_LIVE_METER_ID not all set");
        return;
    };
    let fx = build_fixture(&url, &sk, &meter, "push").await;
    let _export = EXPORT_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // CURRENT month — so `now` (the event timestamp) falls inside the day-aligned
    // [period.start, period.end) window the summary query uses (the REAL-API
    // divergence from the mock's far-future synthetic buckets). Also keeps the
    // event timestamp inside Stripe's [now−35d, now+5min] acceptance window.
    let period = current_period_start_unix();

    let creator = make_user(&fx.state, "push").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    let cus = make_real_customer(&fx, creator, "stripelive_push").await;
    eprintln!("[live] event_name={} meter={} customer={cus} period={period}", fx.event_name, fx.meter_id);

    ingest_at(&fx.state, app, 750, period, 1).await; // 750 requests × 1 CU = 750 CU

    let n = metering_export::tick_at(&fx.state, period, now_unix())
        .await
        .expect("tick");
    assert_eq!(n, 1, "one app exported to LIVE Stripe");

    // High-water advanced locally (the push returned 2xx → the cron committed it).
    let hw = read_high_water(&fx.state, &creator, period).await;
    assert_eq!(hw, Some(750), "exported_units high-water == cumulative CU after a 2xx push");

    // LIVE aggregate converges to 750 (eventual consistency on Stripe's side).
    let agg = poll_aggregate(&fx, &cus, period, 750, 180).await;
    assert_eq!(
        agg, 750,
        "LIVE Stripe meter aggregate == 750 CU for the customer/period (meter_event accepted + reconciled). \
         If this is < 750 the push may have been rejected (timestamp window / payload shape) or the \
         aggregation never converged — see stderr."
    );
}

// ===========================================================================
// Test 2 — delta export across two ticks, reconciled via the LIVE aggregate.
// ===========================================================================

/// Two ticks: tick 1 exports N, usage grows to M, tick 2 exports the DELTA M−N
/// (driven by `reported_total` reading the LIVE Stripe aggregate), and Stripe
/// SUMS to M. Proves the delta math feeds off the REAL aggregate, exactly-once —
/// the no-double-push guarantee against a real meter.
#[compio::test]
#[ignore = "requires a live Stripe TEST key (STRIPE_LIVE_SECRET_KEY) + meter (STRIPE_LIVE_METER_ID) + PG (CONTROL_TEST_DB); run via tests/e2e_stripe_meters_export.sh"]
async fn live_export_computes_delta_via_real_aggregate() {
    let Some((url, sk, meter)) = live_env() else {
        eprintln!("skip: CONTROL_TEST_DB / STRIPE_LIVE_SECRET_KEY / STRIPE_LIVE_METER_ID not all set");
        return;
    };
    let fx = build_fixture(&url, &sk, &meter, "delta").await;
    let _export = EXPORT_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let period = current_period_start_unix();

    let creator = make_user(&fx.state, "delta").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    let cus = make_real_customer(&fx, creator, "stripelive_delta").await;

    ingest_at(&fx.state, app, 100, period, 1).await;
    let n1 = metering_export::tick_at(&fx.state, period, now_unix()).await.expect("tick 1");
    assert_eq!(n1, 1);
    // Let the first delta (100) land in Stripe's aggregate before driving tick 2 —
    // the delta math reads `reported_total` (the LIVE aggregate), so it must
    // reflect tick 1 first (otherwise tick 2 would re-push 100 with a NEW
    // identifier and Stripe SUMs to 350). This poll is the eventual-consistency
    // guard — the divergence the synchronous mock never needs.
    let after1 = poll_aggregate(&fx, &cus, period, 100, 180).await;
    assert_eq!(after1, 100, "aggregate reflects tick 1 (100) before tick 2");

    ingest_at(&fx.state, app, 150, period, 2).await; // grows to 250

    let n2 = metering_export::tick_at(&fx.state, period, now_unix()).await.expect("tick 2");
    assert_eq!(n2, 1, "the second tick pushes the (non-zero) delta");

    let after2 = poll_aggregate(&fx, &cus, period, 250, 180).await;
    assert_eq!(
        after2, 250,
        "LIVE Stripe aggregate SUM == cumulative 250 (tick 2 pushed the DELTA 150, not 250)"
    );
    assert_eq!(read_high_water(&fx.state, &creator, period).await, Some(250));
}

// ===========================================================================
// Test 3 — M1: BILLABLE CU (gross − included) flows through to LIVE Stripe.
// ===========================================================================

/// For a plan with non-zero `included_units`, the export pushes BILLABLE CU
/// (`gross − included`) — the SAME quantity the spend cap treats as billable —
/// and the LIVE Stripe aggregate reflects exactly that, not the gross total.
#[compio::test]
#[ignore = "requires a live Stripe TEST key (STRIPE_LIVE_SECRET_KEY) + meter (STRIPE_LIVE_METER_ID) + PG (CONTROL_TEST_DB); run via tests/e2e_stripe_meters_export.sh"]
async fn live_export_pushes_billable_cu_honoring_included_units() {
    let Some((url, sk, meter)) = live_env() else {
        eprintln!("skip: CONTROL_TEST_DB / STRIPE_LIVE_SECRET_KEY / STRIPE_LIVE_METER_ID not all set");
        return;
    };
    let fx = build_fixture(&url, &sk, &meter, "m1").await;
    let _export = EXPORT_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let period = current_period_start_unix();

    let creator = make_user(&fx.state, "m1").await;
    let included: i64 = 200; // gross 750 ⇒ billable 550
    let plan = make_plan_with_included(&fx.state, included).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    let cus = make_real_customer(&fx, creator, "stripelive_m1").await;

    ingest_at(&fx.state, app, 750, period, 1).await;

    let n = metering_export::tick_at(&fx.state, period, now_unix()).await.expect("tick");
    assert_eq!(n, 1);

    let agg = poll_aggregate(&fx, &cus, period, 550, 180).await;
    assert_eq!(
        agg, 550,
        "LIVE Stripe counted the BILLABLE 550 CU (gross 750 − included 200), not gross"
    );
    assert_eq!(read_high_water(&fx.state, &creator, period).await, Some(550));
}

// ===========================================================================
// Test 4 — C1 (REAL): a future-stamped (period.end) meter event is REJECTED.
// ===========================================================================

/// C1 against REAL Stripe: the provider MUST stamp the event at the consumption
/// instant `now`, NEVER at `period.end`. Here we PROVE the real-API constraint
/// the C1 fix targets — that REAL Stripe rejects a future-stamped event — by
/// pushing the SAME meter_event the export would, but stamped at a far-future
/// `period.end` (> now + 5min). REAL Stripe MUST reject it (HTTP 400,
/// timestamp out of range), and the aggregate MUST NOT move. This is the hard
/// proof (not a mock re-implementation) that a `period.end`-stamped push is a
/// $0-revenue black hole — i.e. the bug the fix closes is real on api.stripe.com.
#[compio::test]
#[ignore = "requires a live Stripe TEST key (STRIPE_LIVE_SECRET_KEY) + meter (STRIPE_LIVE_METER_ID) + PG (CONTROL_TEST_DB); run via tests/e2e_stripe_meters_export.sh"]
async fn live_future_timestamp_meter_event_is_rejected_by_real_stripe() {
    let Some((url, sk, meter)) = live_env() else {
        eprintln!("skip: CONTROL_TEST_DB / STRIPE_LIVE_SECRET_KEY / STRIPE_LIVE_METER_ID not all set");
        return;
    };
    let fx = build_fixture(&url, &sk, &meter, "c1").await;
    let _export = EXPORT_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let period = current_period_start_unix();

    let creator = make_user(&fx.state, "c1").await;
    let cus = make_real_customer(&fx, creator, "stripelive_c1").await;

    let client = StripeClient::new(SecretString::new(fx.secret_key.clone()))
        .with_base_url(STRIPE_LIVE_BASE_URL.to_string());

    // The C1 BUG timestamp: `period.end` (the first of NEXT month) — weeks in the
    // future, well beyond Stripe's +5min skew tolerance. This is EXACTLY what the
    // pre-fix code stamped.
    let future_ts = period_end_unix(period);
    let now = now_unix();
    assert!(
        future_ts > now + 5 * 60,
        "sanity: period.end ({future_ts}) must be > now+5min ({}) to exercise the future-reject",
        now + 5 * 60
    );

    let bug_identifier = format!("livetest-c1-future-{}", Uuid::new_v4().simple());
    let bug_push = client
        .create_meter_event(&fx.event_name, &cus, 999, &bug_identifier, future_ts)
        .await;
    assert!(
        bug_push.is_err(),
        "REAL Stripe MUST reject a future-stamped (period.end) meter event — if this is Ok, the \
         real-API timestamp window assumption (the C1 bug-class) is WRONG; got {bug_push:?}"
    );
    eprintln!("[live][C1] real Stripe rejected the future-stamped push as expected: {bug_push:?}");

    // The aggregate did NOT move (the rejected event was not counted).
    let agg = poll_aggregate(&fx, &cus, period, 1, 12).await;
    assert_eq!(
        agg, 0,
        "the future-stamped (period.end) event was rejected by REAL Stripe → aggregate stays 0 \
         (a $0-revenue black hole if the export had stamped at period.end)"
    );

    // And the CORRECT push (stamped at `now`, what the fix does) IS accepted.
    let good_identifier = format!("livetest-c1-now-{}", Uuid::new_v4().simple());
    client
        .create_meter_event(&fx.event_name, &cus, 42, &good_identifier, now)
        .await
        .expect("a now-stamped meter event MUST be accepted by REAL Stripe (the C1 fix)");
    let agg_after = poll_aggregate(&fx, &cus, period, 42, 180).await;
    assert_eq!(
        agg_after, 42,
        "the now-stamped event (the C1 fix) IS accepted and aggregated by REAL Stripe"
    );
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
