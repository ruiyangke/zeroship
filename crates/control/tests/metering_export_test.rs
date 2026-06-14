//! Integration tests for the metering-export cron + the Stripe Billing Meters
//! provider (M-Stripe, blueprint §M3/§M5/§M8).
//!
//! FAITHFUL by construction: the tests drive the REAL `cyper`-based
//! [`StripeProvider`]/[`StripeClient`] against a localhost **mock-meters HTTP
//! server** (a small HTTP/1.1 server stood up in-test on
//! `compio::net::TcpListener` that speaks Stripe's meter-events form protocol
//! and RECORDS every request). There is NO stubbed client on the wire path: the
//! export cron reads `state.metering_provider` (a real `StripeProvider`) whose
//! base URL points at the mock, so the form encoding, the
//! `Authorization: Bearer` + `Idempotency-Key` headers, the HTTP round-trip and
//! the JSON parse are all exercised end to end. The assertions are on the
//! recorded `meter_events` (value, identifier, customer, headers).
//!
//! Real Postgres via `CONTROL_TEST_DB`; silent skip otherwise. The DB must have
//! changeset 0043 (`metering_exports`) applied.

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use uuid::Uuid;

use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::cron::metering_export;
use zeroship_control::metering::provider::{
    build_provider, MeteringProviderConfig, StripeMeterConfig,
};
use zeroship_control::metering::Metering;
use zeroship_control::{
    AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_core::types::{AppUsage, UsageReport};

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const METER_EVENT_NAME: &str = "compute_units";

fn tmpdir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("zs-mexp-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&p).expect("mk tmpdir");
    p
}

// ===========================================================================
// Mock-meters HTTP server — a real localhost server the REAL cyper client hits.
// ===========================================================================

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    idempotency_key: Option<String>,
    authorization: Option<String>,
    body: String,
}

#[derive(Default)]
struct MockState {
    requests: Vec<RecordedRequest>,
    /// Identifier → the JSON response previously returned. A faithful Stripe
    /// dedups a repeated meter-event `identifier` (within its window): the
    /// SECOND push with the same identifier is NOT counted again. We replay the
    /// original response and DO NOT record it as a fresh created event.
    seen_identifiers: HashMap<String, String>,
}

#[derive(Clone)]
struct MockMeters {
    state: Arc<Mutex<MockState>>,
    base_url: String,
}

impl MockMeters {
    fn requests(&self) -> Vec<RecordedRequest> {
        self.state.lock().unwrap().requests.clone()
    }

    /// All recorded `POST /v1/billing/meter_events` requests.
    fn meter_events(&self) -> Vec<RecordedRequest> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == "POST" && r.path.starts_with("/v1/billing/meter_events"))
            .collect()
    }
}

async fn start_mock_meters() -> MockMeters {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().expect("local_addr");
    let base_url = format!("http://{addr}");
    let state = Arc::new(Mutex::new(MockState::default()));
    let accept_state = Arc::clone(&state);

    compio::runtime::spawn(async move {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else { break };
            let conn_state = Arc::clone(&accept_state);
            compio::runtime::spawn(async move {
                serve_conn(stream, conn_state).await;
            })
            .detach();
        }
    })
    .detach();

    MockMeters { state, base_url }
}

async fn serve_conn(mut stream: TcpStream, state: Arc<Mutex<MockState>>) {
    let mut acc: Vec<u8> = Vec::new();
    loop {
        loop {
            let Some((req, consumed)) = try_parse_request(&acc) else { break };
            acc.drain(0..consumed);
            let response = handle_mock_request(&req, &state);
            if stream.write_all(response).await.0.is_err() {
                return;
            }
        }
        let buf = vec![0u8; 4096];
        let compio::BufResult(n, buf) = stream.read(buf).await;
        match n {
            Ok(0) | Err(_) => return,
            Ok(read) => acc.extend_from_slice(&buf[..read]),
        }
    }
}

fn try_parse_request(buf: &[u8]) -> Option<(RecordedRequest, usize)> {
    let text = std::str::from_utf8(buf).ok()?;
    let header_end = text.find("\r\n\r\n")?;
    let head = &text[..header_end];
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut content_length = 0usize;
    let mut idempotency_key = None;
    let mut authorization = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_string();
            match key.as_str() {
                "content-length" => content_length = val.parse().unwrap_or(0),
                "idempotency-key" => idempotency_key = Some(val),
                "authorization" => authorization = Some(val),
                _ => {}
            }
        }
    }

    let body_start = header_end + 4;
    if buf.len() < body_start + content_length {
        return None;
    }
    let body = String::from_utf8_lossy(&buf[body_start..body_start + content_length]).to_string();

    Some((
        RecordedRequest {
            method,
            path,
            idempotency_key,
            authorization,
            body,
        },
        body_start + content_length,
    ))
}

/// Answer a Stripe meter-event create with a meter-event-shaped JSON 200,
/// recording every request. Faithful dedupe: a repeated `identifier` replays
/// the original response and is NOT re-recorded as a fresh event (so the test
/// can assert the meter SUMMED each delta exactly once).
fn handle_mock_request(req: &RecordedRequest, state: &Arc<Mutex<MockState>>) -> Vec<u8> {
    if req.method == "POST" && req.path.starts_with("/v1/billing/meter_events") {
        let identifier = form_param(&req.body, "identifier").unwrap_or_default();
        let mut st = state.lock().unwrap();
        if let Some(prev) = st.seen_identifiers.get(&identifier).cloned() {
            // Stripe dedups a repeated identifier — replay, do NOT record again.
            return http_200_json(&prev);
        }
        let json = format!(
            r#"{{"object":"billing.meter_event","identifier":"{identifier}"}}"#
        );
        st.seen_identifiers.insert(identifier, json.clone());
        st.requests.push(req.clone());
        return http_200_json(&json);
    }
    if req.method == "POST" && req.path.starts_with("/v1/customers") {
        let json = format!(r#"{{"id":"cus_mock_{}","object":"customer"}}"#, short());
        state.lock().unwrap().requests.push(req.clone());
        return http_200_json(&json);
    }
    state.lock().unwrap().requests.push(req.clone());
    http_200_json(r#"{"id":"obj_mock","object":"unknown"}"#)
}

fn form_param(body: &str, name: &str) -> Option<String> {
    for pair in body.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if percent_decode(k) == name {
                return Some(percent_decode(v));
            }
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn http_200_json(json: &str) -> Vec<u8> {
    let body = json.to_string().into_bytes();
    let mut resp = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n",
        body.len()
    )
    .into_bytes();
    resp.extend_from_slice(&body);
    resp
}

fn short() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_string()
}

// ===========================================================================
// Fixture (real PG + a real StripeProvider pointed at the mock-meters server).
// ===========================================================================

struct Fixture {
    state: Arc<AppState>,
    mock: MockMeters,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

async fn build_fixture(db_url: &str, label: &str) -> Fixture {
    let mock = start_mock_meters().await;
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

    // THE faithful seam: a REAL StripeProvider whose base URL is the mock-meters
    // server. The export cron pushes through the real cyper client over the wire.
    let provider = build_provider(&MeteringProviderConfig::stripe(StripeMeterConfig {
        event_name: METER_EVENT_NAME.to_string(),
        secret_key: SecretString::new("sk_test_mock".to_string()),
        base_url: mock.base_url.clone(),
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
        stripe_secret_key: SecretString::new("sk_test_mock".to_string()),
        stripe_base_url: mock.base_url.clone(),
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
        pairwise_salt: [0u8; 32],
    });

    Fixture {
        state,
        mock,
        blob_root,
        deploy_tmp_dir,
    }
}

// ---------------------------------------------------------------------------
// DB seeding helpers (mirror billing_reconcile_test).
// ---------------------------------------------------------------------------

async fn make_user(state: &AppState, label: &str) -> Uuid {
    let email = format!("{label}-{}@example.test", Uuid::new_v4().simple());
    let rows = state
        .control_pg
        .query(
            "INSERT INTO zeroship.users (email, name) VALUES ($1, $2) RETURNING id",
            &[&email, &"Test Creator".to_string()],
        )
        .await
        .expect("insert user");
    rows[0].get("id")
}

/// Seed the global `requests` weight = 1 CU/op (the export pushes raw CU; FX is
/// Stripe's price on this rail). Returns a plan id charging 1c/req locally so the
/// spend cap can be exercised for the CU-parity test.
async fn make_plan(state: &AppState) -> String {
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
    let plan_id = format!("pln_mexp_{}", Uuid::new_v4().simple());
    let fx_one_cent: i64 = 1_000_000_000_000;
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'mexp-test', 0, 0, $2, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', 100000)",
            &[&plan_id, &fx_one_cent],
        )
        .await
        .expect("seed priced plan");
    plan_id
}

async fn make_owned_app(state: &AppState, plan_id: &str, owner: Uuid) -> Uuid {
    let name = format!("mexp-{}", Uuid::new_v4());
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

/// Ingest usage at an explicit period_start (the period the export sweeps).
async fn ingest_at(state: &AppState, app: Uuid, requests: u64, period_start: i64, seq: u64) {
    let metering = Metering::new(state.registry.clone());
    let worker = format!("w-{}", Uuid::new_v4());
    metering
        .ingest_at(&report(&worker, seq, app, requests), period_start)
        .await
        .expect("ingest usage");
}

/// A FIXED period start (a calendar-month boundary) so the test is deterministic
/// and isolated from "now". May 2026.
fn fixed_period() -> i64 {
    chrono::TimeZone::timestamp_opt(&chrono::Utc, 1_746_057_600, 0) // 2026-05-01T00:00:00Z
        .single()
        .unwrap()
        .timestamp()
}

// ===========================================================================
// Tests.
// ===========================================================================

/// `report_usage` (via the export cron) pushes a `meter_event` with the correct
/// CU value + customer + deterministic identifier — through the REAL cyper
/// client hitting the mock-meters server. Asserts on the recorded event.
#[compio::test]
async fn export_pushes_cu_as_meter_event_with_correct_value_customer_and_identifier() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "push").await;
    let period = fixed_period();

    let creator = make_user(&fx.state, "push").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, "cus_push").await.unwrap();

    ingest_at(&fx.state, app, 750, period, 1).await; // 750 requests × 1 CU = 750 CU

    let n = metering_export::tick_at(&fx.state, period).await.expect("tick");
    assert_eq!(n, 1, "one app exported");

    let events = fx.mock.meter_events();
    assert_eq!(events.len(), 1, "exactly one meter_event pushed");
    let ev = &events[0];
    // Bearer auth + the identifier-as-idempotency-key on the wire.
    assert_eq!(ev.authorization.as_deref(), Some("Bearer sk_test_mock"), "bearer auth sent");
    // The form body carries event_name + payload[value]=CU + payload[stripe_customer_id].
    assert!(ev.body.contains(&format!("event_name={METER_EVENT_NAME}")), "event_name; body={}", ev.body);
    assert!(ev.body.contains("payload%5Bvalue%5D=750"), "payload[value]=750 (the CU); body={}", ev.body);
    assert!(
        ev.body.contains("payload%5Bstripe_customer_id%5D=cus_push"),
        "payload[stripe_customer_id]=cus_push; body={}", ev.body
    );
    // The deterministic identifier for the (app, period, 0→750) window, present
    // on the wire (form `identifier=…`, percent-encoded colons) AND verbatim as
    // the Idempotency-Key header.
    let expected_id = metering_export::export_identifier(&app, period, 0, 750);
    assert_eq!(
        form_param(&ev.body, "identifier").as_deref(),
        Some(expected_id.as_str()),
        "deterministic identifier on the wire; body={}", ev.body
    );
    assert_eq!(ev.idempotency_key.as_deref(), Some(expected_id.as_str()), "identifier doubles as Idempotency-Key");

    // The high-water advanced to the cumulative total.
    let hw = read_high_water(&fx.state, &app, period).await;
    assert_eq!(hw, Some(750), "exported_units high-water == cumulative CU");
}

/// THE no-double-push guarantee (RED→GREEN on the `metering_exports`
/// high-water). Run the export tick TWICE with NO new usage between them. The
/// second tick must be a pure no-op: delta 0 ⇒ NO second meter_event.
///
/// RED→GREEN: WITHOUT the high-water (if every tick pushed the cumulative
/// total), the second tick would push 750 AGAIN — and because the window (and
/// thus the identifier) is unchanged the mock dedups it, but a CUMULATIVE
/// re-push with a fresh identifier would DOUBLE-count. The high-water makes the
/// delta 0 so nothing is even sent. We assert the mock recorded the event
/// EXACTLY ONCE across both ticks.
#[compio::test]
async fn second_export_tick_with_no_new_usage_is_a_noop() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "noop").await;
    let period = fixed_period();

    let creator = make_user(&fx.state, "noop").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, "cus_noop").await.unwrap();
    ingest_at(&fx.state, app, 300, period, 1).await;

    let n1 = metering_export::tick_at(&fx.state, period).await.expect("tick 1");
    assert_eq!(n1, 1, "first tick exports the app");
    assert_eq!(fx.mock.meter_events().len(), 1, "one event after the first tick");

    // Second tick, SAME usage — must be a no-op (delta 0).
    let n2 = metering_export::tick_at(&fx.state, period).await.expect("tick 2");
    assert_eq!(n2, 0, "second tick is a no-op (high-water == cumulative ⇒ delta 0)");
    assert_eq!(
        fx.mock.meter_events().len(),
        1,
        "no double-push: the meter_event was sent exactly once across two ticks (high-water guard)"
    );
}

/// The export cron computes the correct DELTA across two ticks: tick 1 exports
/// N, usage grows to M, tick 2 exports M−N (NOT M). Stripe SUMS the two events
/// to M — the correct cumulative — proving delta export, not cumulative re-push.
#[compio::test]
async fn export_computes_delta_across_two_ticks() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "delta").await;
    let period = fixed_period();

    let creator = make_user(&fx.state, "delta").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, "cus_delta").await.unwrap();

    // Tick 1: N = 100 CU.
    ingest_at(&fx.state, app, 100, period, 1).await;
    let n1 = metering_export::tick_at(&fx.state, period).await.expect("tick 1");
    assert_eq!(n1, 1);

    // Usage grows to M = 250 CU (a +150 increment in a fresh report).
    ingest_at(&fx.state, app, 150, period, 2).await;

    // Tick 2: must push the DELTA M−N = 150 (not 250).
    let n2 = metering_export::tick_at(&fx.state, period).await.expect("tick 2");
    assert_eq!(n2, 1, "the second tick pushes the (non-zero) delta");

    let events = fx.mock.meter_events();
    assert_eq!(events.len(), 2, "two meter_events: the initial export + the delta");
    assert!(events[0].body.contains("payload%5Bvalue%5D=100"), "tick 1 pushed 100; body={}", events[0].body);
    assert!(
        events[1].body.contains("payload%5Bvalue%5D=150"),
        "tick 2 pushed the DELTA 150, NOT the cumulative 250; body={}", events[1].body
    );
    // The Stripe-side SUM (100 + 150) == the cumulative 250 — and == the local CU.
    let hw = read_high_water(&fx.state, &app, period).await;
    assert_eq!(hw, Some(250), "high-water advanced to the new cumulative 250");

    // CU PARITY: the cumulative CU the export pushed == the CU the spend cap uses
    // (`total_units` over the SAME period_totals). Single source of truth.
    let metering = Metering::new(fx.state.registry.clone());
    let totals = metering.period_totals(&app, period).await.expect("period totals");
    let weights = zeroship_control::pricing_store::PricingStore::new(fx.state.registry.clone())
        .weights()
        .await
        .expect("weights");
    let cap_cu = zeroship_control::pricing::total_units(&weights, &totals).expect("total_units");
    assert_eq!(cap_cu, 250, "the spend cap's CU == the cumulative CU exported (parity)");
}

/// Read the `metering_exports` high-water for an `(app, period)`.
async fn read_high_water(state: &AppState, app: &Uuid, period: i64) -> Option<i64> {
    state
        .control_pg
        .query(
            "SELECT exported_units FROM zeroship.metering_exports \
             WHERE app_id = $1 AND period_start = to_timestamp($2::double precision)",
            &[app, &(period as f64)],
        )
        .await
        .expect("read high-water")
        .first()
        .map(|r| r.get::<_, i64>("exported_units"))
}
