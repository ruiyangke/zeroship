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
const METER_ID: &str = "mtr_test_compute_units";

/// Real Stripe rejects a meter-event `timestamp` more than 5 minutes in the
/// FUTURE or older than 35 days in the PAST. The mock enforces the SAME window so
/// a future-stamped push (the C1 bug, which stamped at `period.end`) is a hard
/// 400 the test can catch — not a silently-accepted event.
const STRIPE_FUTURE_SKEW_SECS: i64 = 5 * 60;
const STRIPE_PAST_WINDOW_SECS: i64 = 35 * 24 * 60 * 60;

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

struct MockState {
    requests: Vec<RecordedRequest>,
    /// Identifier → the JSON response previously returned. A faithful Stripe
    /// dedups a repeated meter-event `identifier` (within its ~24h window): the
    /// SECOND push with the same identifier is NOT counted again. We replay the
    /// original response and DO NOT record it as a fresh created event nor add it
    /// to the aggregate.
    seen_identifiers: HashMap<String, String>,
    /// Whether the `identifier` dedup window is active. Real Stripe expires the
    /// window after ~24h; `disable_dedupe()` flips this OFF to simulate a >24h
    /// re-drive (the C2 scenario), where a repeated identifier is SUMMED again
    /// unless the caller's own reconcile (against the aggregate) prevents it.
    dedupe_enabled: bool,
    /// The SUM of every ACCEPTED meter-event `payload[value]` per
    /// `stripe_customer_id` — the meter's aggregate, served back by the
    /// `event_summaries` endpoint. This is the authoritative "what Stripe has
    /// counted" the C2 reconcile reads. A deduped or rejected event does NOT add.
    accepted_value_by_customer: HashMap<String, u64>,
    /// When true, EVERY meter-event push is rejected with a 400 (simulating a
    /// permanently mis-provisioned meter / customer) — drives the M2 durable
    /// failure-surface test.
    reject_meter_events: bool,
}

impl Default for MockState {
    fn default() -> Self {
        Self {
            requests: Vec::new(),
            seen_identifiers: HashMap::new(),
            dedupe_enabled: true, // faithful default: dedup a repeated identifier
            accepted_value_by_customer: HashMap::new(),
            reject_meter_events: false,
        }
    }
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

    /// All ACCEPTED `POST /v1/billing/meter_events` requests (deduped + rejected
    /// pushes are NOT recorded here — so the count is the meter events Stripe
    /// actually counted).
    fn meter_events(&self) -> Vec<RecordedRequest> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == "POST" && r.path.starts_with("/v1/billing/meter_events"))
            .collect()
    }

    /// Turn OFF the `identifier` dedup window to simulate a >24h re-drive (the C2
    /// scenario): a repeated identifier is no longer replayed — Stripe would SUM
    /// it again. Only the caller's reconcile-against-aggregate can then prevent a
    /// double-count.
    fn disable_dedupe(&self) {
        self.state.lock().unwrap().dedupe_enabled = false;
    }

    /// Reject EVERY subsequent meter-event push with a 400 (a permanently
    /// mis-provisioned meter) — drives the M2 durable-failure test.
    fn reject_all_meter_events(&self) {
        self.state.lock().unwrap().reject_meter_events = true;
    }

    /// Allow meter-event pushes again (recovery path for the M2 reset assertion).
    fn allow_meter_events(&self) {
        self.state.lock().unwrap().reject_meter_events = false;
    }

    /// The meter's aggregated value Stripe has accepted for `customer` (the SUM
    /// the `event_summaries` endpoint serves). The C2 reconcile reads this.
    fn aggregate_for(&self, customer: &str) -> u64 {
        self.state
            .lock()
            .unwrap()
            .accepted_value_by_customer
            .get(customer)
            .copied()
            .unwrap_or(0)
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

/// The mock's wall-clock "now" (unix seconds), used to enforce Stripe's
/// timestamp-acceptance window on a meter-event push.
fn mock_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Answer a Stripe meter-event create with a meter-event-shaped JSON 200,
/// recording every ACCEPTED request. Faithful Stripe semantics:
///   * `timestamp` window — REJECT (HTTP 400) a timestamp more than 5min in the
///     future or older than 35 days (this is what makes C1, which stamped at the
///     future `period.end`, a hard failure rather than a silently-lost event);
///   * dedup window — a repeated `identifier` is replayed and NOT re-counted
///     while `dedupe_enabled`; with dedup OFF (a >24h re-drive) the repeat is
///     SUMMED again (the C2 hazard the caller's reconcile must defuse);
///   * aggregate — an ACCEPTED (non-deduped) event's `payload[value]` is added
///     to the per-customer running total served by `event_summaries`.
/// Also serves `GET /v1/billing/meters/{id}/event_summaries` with that total.
fn handle_mock_request(req: &RecordedRequest, state: &Arc<Mutex<MockState>>) -> Vec<u8> {
    if req.method == "POST" && req.path.starts_with("/v1/billing/meter_events") {
        let identifier = form_param(&req.body, "identifier").unwrap_or_default();
        let customer =
            form_param(&req.body, "payload[stripe_customer_id]").unwrap_or_default();
        let value: u64 = form_param(&req.body, "payload[value]")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let ts: i64 = form_param(&req.body, "timestamp")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        // C1: enforce Stripe's timestamp-acceptance window. A future-stamped push
        // (the period.end bug) is a hard 400 — the event is NOT counted.
        let now = mock_now();
        if ts > now + STRIPE_FUTURE_SKEW_SECS || ts < now - STRIPE_PAST_WINDOW_SECS {
            return http_400_json(
                "timestamp_out_of_range",
                "timestamp is outside the allowed range (past 35 days … +5min future)",
            );
        }

        let mut st = state.lock().unwrap();
        if st.reject_meter_events {
            // Simulate a permanent 4xx (mis-provisioned meter / customer): NOT
            // counted, NOT recorded — the push fails.
            return http_400_json(
                "resource_missing",
                "no such meter / customer for this meter event",
            );
        }
        if st.dedupe_enabled {
            if let Some(prev) = st.seen_identifiers.get(&identifier).cloned() {
                // Within the dedup window: replay, do NOT count again.
                return http_200_json(&prev);
            }
        }
        let json = format!(r#"{{"object":"billing.meter_event","identifier":"{identifier}"}}"#);
        st.seen_identifiers.insert(identifier, json.clone());
        // ACCEPTED → add to the per-customer aggregate the meter would report.
        *st.accepted_value_by_customer.entry(customer).or_insert(0) += value;
        st.requests.push(req.clone());
        return http_200_json(&json);
    }
    if req.method == "GET" && req.path.contains("/event_summaries") {
        // Serve the meter's aggregate for the queried customer. Shape mirrors
        // Stripe's `event_summaries` list: one summary row carrying the SUM.
        let customer = query_param(&req.path, "customer").unwrap_or_default();
        let total = state
            .lock()
            .unwrap()
            .accepted_value_by_customer
            .get(&customer)
            .copied()
            .unwrap_or(0);
        state.lock().unwrap().requests.push(req.clone());
        let json = format!(
            r#"{{"object":"list","data":[{{"object":"billing.meter_event_summary","aggregated_value":{total}}}]}}"#
        );
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

/// Extract a query-string parameter from a request path (percent-decoded).
fn query_param(path: &str, name: &str) -> Option<String> {
    let qs = path.split_once('?')?.1;
    for pair in qs.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if percent_decode(k) == name {
                return Some(percent_decode(v));
            }
        }
    }
    None
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

/// A Stripe-shaped error body with an HTTP 400 (what `post_form` maps to
/// `StripeError::Api { status: 400, code }`).
fn http_400_json(code: &str, message: &str) -> Vec<u8> {
    let body = format!(r#"{{"error":{{"code":"{code}","message":"{message}"}}}}"#).into_bytes();
    let mut resp = format!(
        "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n",
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
        meter_id: METER_ID.to_string(),
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
    make_plan_with_included(state, 0).await
}

/// As [`make_plan`] but with a non-zero `included_units` quota — used by the M1
/// parity test to prove the export pushes BILLABLE CU (`gross − included`).
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
    let plan_id = format!("pln_mexp_{}", Uuid::new_v4().simple());
    let fx_one_cent: i64 = 1_000_000_000_000;
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'mexp-test', 0, $2, $3, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', 100000)",
            &[&plan_id, &included_units, &fx_one_cent],
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

/// A DISTINCT, isolated calendar-month period bucket keyed off `year_month`
/// (e.g. `(2031, 3)`). Each new test uses its OWN month so the FLEET-WIDE export
/// sweep (which keys on `period_start`) sees only that test's app — making the
/// per-tick exported-count `n` deterministic regardless of test ordering on the
/// shared DB. A far-future month also keeps `period.end` in the future (so the
/// C1 bug — stamping at `period.end` — is a future-timestamp the mock rejects).
fn month_period(year: i32, month: u32) -> i64 {
    use chrono::TimeZone;
    chrono::Utc
        .with_ymd_and_hms(year, month, 1, 0, 0, 0)
        .single()
        .expect("valid month period")
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

    let n = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("tick");
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

    let n1 = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("tick 1");
    assert_eq!(n1, 1, "first tick exports the app");
    assert_eq!(fx.mock.meter_events().len(), 1, "one event after the first tick");

    // Second tick, SAME usage — must be a no-op (delta 0).
    let n2 = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("tick 2");
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
    let n1 = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("tick 1");
    assert_eq!(n1, 1);

    // Usage grows to M = 250 CU (a +150 increment in a fresh report).
    ingest_at(&fx.state, app, 150, period, 2).await;

    // Tick 2: must push the DELTA M−N = 150 (not 250).
    let n2 = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("tick 2");
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

// ===========================================================================
// C1 — future timestamp = $0-revenue black hole.
// ===========================================================================

/// C1 (RED→GREEN): the meter event must be stamped at the CONSUMPTION instant
/// (`now`), NEVER at `period.end` (the first of NEXT month — a FUTURE timestamp
/// Stripe rejects). The hardened mock enforces Stripe's window: a timestamp more
/// than 5min in the future is a hard 400, so the event is NOT counted.
///
/// We sweep the CURRENT period, whose `period.end` is genuinely weeks in the
/// future. WITH the bug (stamp at period.end) the mock 400s ⇒ ZERO accepted
/// events ⇒ a silent $0 black hole. WITH the fix (stamp at `now`) the event is
/// accepted and lands inside [now−35d, now+5min].
#[compio::test]
async fn export_stamps_event_at_now_not_future_period_end() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "c1ts").await;
    // A far-future, ISOLATED period — its `period.end` is in the future (so the
    // C1 bug, stamping at period.end, would be a rejected future timestamp). The
    // event itself is stamped at `now` (today), inside Stripe's window.
    let period = month_period(2031, 1);

    let creator = make_user(&fx.state, "c1ts").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, "cus_c1ts").await.unwrap();

    ingest_at(&fx.state, app, 500, period, 1).await;

    let now = mock_now();
    let n = metering_export::tick_at(&fx.state, period, now).await.expect("tick");
    assert_eq!(n, 1, "the app exported (the future-timestamp push was NOT rejected)");

    let events = fx.mock.meter_events();
    assert_eq!(
        events.len(),
        1,
        "exactly one ACCEPTED meter_event — a period.end (future) timestamp would 400 and record ZERO"
    );
    // The accepted event's timestamp is the consumption instant, inside Stripe's
    // acceptance window — and is NOT the future period.end.
    let ts: i64 = form_param(&events[0].body, "timestamp")
        .and_then(|v| v.parse().ok())
        .expect("timestamp on the wire");
    assert!(
        ts >= now - STRIPE_PAST_WINDOW_SECS && ts <= now + STRIPE_FUTURE_SKEW_SECS,
        "event timestamp {ts} must be within Stripe's [now−35d, now+5min] window (now={now})"
    );

    // Stripe's aggregate reflects the accepted CU (not lost).
    assert_eq!(fx.mock.aggregate_for("cus_c1ts"), 500, "Stripe counted the 500 CU");
}

// ===========================================================================
// C2 — >24h over-bill residual: crash-after-push, re-drive past the dedup
// window, reconcile against Stripe's aggregate ⇒ NO double-count.
// ===========================================================================

/// C2 (RED→GREEN): a crash AFTER the Stripe push but BEFORE the high-water
/// UPDATE leaves the local high-water stale. Re-driven PAST Stripe's ~24h
/// `identifier` dedup window (mock dedupe DISABLED), the OLD code re-pushes
/// `current − high_water` with a FRESH-enough state that Stripe SUMS it twice
/// (double-bill). The FIX reconciles against Stripe's AGGREGATED meter value:
/// `delta = current − max(high_water, stripe_aggregate)` ⇒ the re-drive pushes
/// nothing, so Stripe's aggregate ends at the correct value, NOT doubled.
///
/// We simulate the crash by manually pushing the meter event (so Stripe's
/// aggregate reflects it) WITHOUT advancing the high-water, then disabling dedup
/// and running the sweep.
#[compio::test]
async fn redrive_past_dedupe_window_does_not_double_count() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "c2redrive").await;
    let period = month_period(2031, 2); // distinct isolated bucket

    let creator = make_user(&fx.state, "c2").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, "cus_c2").await.unwrap();

    ingest_at(&fx.state, app, 400, period, 1).await;

    // First sweep: pushes 400, lands in Stripe's aggregate, advances high-water.
    let now = mock_now();
    let n1 = metering_export::tick_at(&fx.state, period, now).await.expect("tick 1");
    assert_eq!(n1, 1);
    assert_eq!(fx.mock.aggregate_for("cus_c2"), 400, "Stripe counted 400 after tick 1");
    assert_eq!(read_high_water(&fx.state, &app, period).await, Some(400));

    // Simulate the crash window: roll the high-water BACK to 0 (as if the push
    // had landed at Stripe but the high-water UPDATE never committed). Stripe's
    // aggregate still holds 400.
    fx.state
        .control_pg
        .execute(
            "UPDATE zeroship.metering_exports SET exported_units = 0 \
             WHERE app_id = $1",
            &[&app],
        )
        .await
        .expect("roll back high-water to simulate crash-before-update");

    // >24h later: Stripe's Idempotency-Key/identifier dedup window has expired.
    fx.mock.disable_dedupe();

    // Re-drive. The OLD code would re-push 400 (current 400 − stale high-water 0)
    // and, with dedup OFF, Stripe would SUM it to 800 (double-bill). The FIX reads
    // Stripe's aggregate (400) and pushes 400 − max(0, 400) = 0.
    let n2 = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("tick 2");

    assert_eq!(
        fx.mock.aggregate_for("cus_c2"),
        400,
        "Stripe's aggregate stays 400 — the >24h re-drive did NOT double-count (reconciled against the aggregate)"
    );
    assert_eq!(n2, 0, "the re-drive pushed nothing (the aggregate already covered current)");
    // The high-water self-heals back up to the reconciled current.
    assert_eq!(read_high_water(&fx.state, &app, period).await, Some(400), "high-water self-healed");
}

// ===========================================================================
// M1 — CU parity: push BILLABLE CU (gross − included), matching charge_cents.
// ===========================================================================

/// M1 (RED→GREEN): for a plan with a NON-ZERO `included_units`, the export must
/// push BILLABLE CU (`gross − included`), the SAME quantity `charge_cents` (and
/// thus the spend cap) treats as billable — NOT the gross `total_units`. The OLD
/// code pushed gross, over-billing Stripe vs local enforcement by `included`.
#[compio::test]
async fn export_pushes_billable_cu_honoring_included_units() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "m1parity").await;
    let period = month_period(2031, 3); // distinct isolated bucket

    let creator = make_user(&fx.state, "m1").await;
    // 200 CU included; gross 750 ⇒ billable 550.
    let included: i64 = 200;
    let plan = make_plan_with_included(&fx.state, included).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, "cus_m1").await.unwrap();

    ingest_at(&fx.state, app, 750, period, 1).await; // gross 750 CU

    let n = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("tick");
    assert_eq!(n, 1);

    let events = fx.mock.meter_events();
    assert_eq!(events.len(), 1);
    // The pushed value is the BILLABLE CU (750 − 200), NOT the gross 750.
    assert!(
        events[0].body.contains("payload%5Bvalue%5D=550"),
        "pushed BILLABLE CU 550 (gross 750 − included 200), NOT gross; body={}",
        events[0].body
    );

    // PARITY: 550 == what charge_cents treats as billable over the SAME totals.
    let metering = Metering::new(fx.state.registry.clone());
    let totals = metering.period_totals(&app, period).await.expect("period totals");
    let weights = zeroship_control::pricing_store::PricingStore::new(fx.state.registry.clone())
        .weights()
        .await
        .expect("weights");
    let gross = zeroship_control::pricing::total_units(&weights, &totals).expect("total_units");
    let billable = gross.saturating_sub(included as u64);
    assert_eq!(billable, 550, "billable parity sanity");
    assert_eq!(fx.mock.aggregate_for("cus_m1"), 550, "Stripe counted the BILLABLE 550");
    // The high-water tracks billable CU (so the next delta is computed on billable).
    assert_eq!(read_high_water(&fx.state, &app, period).await, Some(550));
}

// ===========================================================================
// M2 — durable per-app export failure surface.
// ===========================================================================

/// M2 (RED→GREEN): a failing `report_usage` must be DURABLE + queryable, not
/// log-only — else a permanently mis-provisioned app under-bills forever
/// invisibly. We make the mock reject every push (a permanent 4xx), sweep, and
/// assert the `metering_exports` failure surface (`consecutive_failures`,
/// `last_error`) advances; a later SUCCESS resets it. WITHOUT the columns +
/// bookkeeping the failure is invisible (RED: the columns don't exist / stay 0).
#[compio::test]
async fn export_failure_is_recorded_durably() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "m2fail").await;
    let period = month_period(2031, 4); // distinct isolated bucket

    let creator = make_user(&fx.state, "m2").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, "cus_m2").await.unwrap();
    ingest_at(&fx.state, app, 300, period, 1).await;

    // Make the mock reject EVERY meter-event push (simulate a permanent 4xx, e.g.
    // a mis-provisioned meter). The failure must be recorded durably.
    fx.mock.reject_all_meter_events();

    let n = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("tick");
    assert_eq!(n, 0, "nothing exported (the push failed)");

    let (failures, last_error) = read_failure_state(&fx.state, &app, period).await;
    assert_eq!(failures, Some(1), "consecutive_failures bumped to 1 on the failed push");
    assert!(
        last_error.as_deref().is_some_and(|e| e.contains("report_usage")),
        "last_error records the failure (got {last_error:?})"
    );
    // The high-water did NOT advance (nothing was exported).
    assert_eq!(read_high_water(&fx.state, &app, period).await, Some(0));

    // A SECOND failed tick bumps the counter again (observability of a stuck app).
    let _ = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("tick 2");
    let (failures2, _) = read_failure_state(&fx.state, &app, period).await;
    assert_eq!(failures2, Some(2), "a second failure bumps consecutive_failures to 2");

    // Now let pushes through; a SUCCESS resets the failure surface.
    fx.mock.allow_meter_events();
    let n3 = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("tick 3");
    assert_eq!(n3, 1, "the recovered push exports");
    let (failures3, last_error3) = read_failure_state(&fx.state, &app, period).await;
    assert_eq!(failures3, Some(0), "a successful export RESETS consecutive_failures to 0");
    assert_eq!(last_error3, None, "last_error cleared on success");
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

/// Read the M2 durable failure surface for an `(app, period)`:
/// `(consecutive_failures, last_error)`.
async fn read_failure_state(
    state: &AppState,
    app: &Uuid,
    period: i64,
) -> (Option<i32>, Option<String>) {
    let rows = state
        .control_pg
        .query(
            "SELECT consecutive_failures, last_error FROM zeroship.metering_exports \
             WHERE app_id = $1 AND period_start = to_timestamp($2::double precision)",
            &[app, &(period as f64)],
        )
        .await
        .expect("read failure state");
    match rows.first() {
        Some(r) => (
            Some(r.get::<_, i32>("consecutive_failures")),
            r.get::<_, Option<String>>("last_error"),
        ),
        None => (None, None),
    }
}
