//! Integration tests for the metering-export cron + the OpenMeter provider
//! (M-OpenMeter, blueprint §M4/§M5/§M8).
//!
//! FAITHFUL by construction: the tests drive the REAL `cyper`-based
//! [`OpenMeterProvider`]/[`OpenMeterClient`] against a localhost **mock-OpenMeter
//! HTTP server** (a small HTTP/1.1 server stood up in-test on
//! `compio::net::TcpListener` that speaks OpenMeter's CloudEvents ingest + meter
//! query protocol and RECORDS every request). There is NO stubbed client on the
//! wire path: the export cron reads `state.metering_provider` (a real
//! `OpenMeterProvider`) whose base URL points at the mock, so the CloudEvents
//! JSON encoding, the `Authorization: Bearer` header, the HTTP round-trip and the
//! JSON parse are all exercised end to end. The assertions are on the recorded
//! CloudEvents (data.value, id, subject) + the aggregate the query serves.
//!
//! The cron under test is the SAME hardened `metering_export` the Stripe rail
//! drives (provider-agnostic delta / C2-reconcile / C1-timestamp / M1-billable
//! logic) — here it is driven through the OpenMeter provider, proving the
//! inherited guarantees hold for OpenMeter too.
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
    build_provider, MeteringProviderConfig, OpenMeterConfig,
};
use zeroship_control::metering::Metering;
use zeroship_control::{
    AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_core::types::{AppUsage, UsageReport};

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

/// `metering_export::tick_at`/`sweep` single-flights fleet-wide via
/// `pg_try_advisory_lock` (production multi-instance safety) — a NON-blocking
/// `try` lock, so two concurrent sweeps would have one return `Ok(0)` (skip),
/// breaking a `n == 1` assertion. Serialize the sweep-driving tests with a
/// process-wide lock to mirror the production single-flight (poison-recovered).
static EXPORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const OM_EVENT_TYPE: &str = "compute_units";
const OM_METER_SLUG: &str = "compute_units";

fn tmpdir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("zs-omexp-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&p).expect("mk tmpdir");
    p
}

// ===========================================================================
// Mock-OpenMeter HTTP server — a real localhost server the REAL cyper client hits.
// ===========================================================================

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    content_type: Option<String>,
    authorization: Option<String>,
    body: String,
}

struct MockState {
    requests: Vec<RecordedRequest>,
    /// CloudEvent `id` → already-seen. A faithful OpenMeter dedups a repeated
    /// `(source, id)` event: the SECOND ingest with the same id is NOT counted
    /// again. We DO NOT record it as a fresh accepted event nor add it to the
    /// aggregate.
    seen_ids: std::collections::HashSet<String>,
    /// Whether the `id` dedup window is active. Real OpenMeter expires the window
    /// after its retention; `disable_dedupe()` flips this OFF to simulate a >24h
    /// re-drive (the C2 scenario), where a repeated id is SUMMED again unless the
    /// caller's own reconcile (against the aggregate) prevents it.
    dedupe_enabled: bool,
    /// The SUM of every ACCEPTED CloudEvent `data.value` per `subject` — the
    /// meter's aggregate, served back by the `/query` endpoint. A deduped or
    /// rejected event does NOT add.
    accepted_value_by_subject: HashMap<String, u64>,
    /// When true, EVERY ingest is rejected with a 400 (simulating a permanently
    /// mis-provisioned meter / subject) — drives the M2 durable failure-surface
    /// test.
    reject_ingest: bool,
}

impl Default for MockState {
    fn default() -> Self {
        Self {
            requests: Vec::new(),
            seen_ids: std::collections::HashSet::new(),
            dedupe_enabled: true, // faithful default: dedup a repeated id
            accepted_value_by_subject: HashMap::new(),
            reject_ingest: false,
        }
    }
}

#[derive(Clone)]
struct MockOpenMeter {
    state: Arc<Mutex<MockState>>,
    base_url: String,
}

impl MockOpenMeter {
    fn requests(&self) -> Vec<RecordedRequest> {
        self.state.lock().unwrap().requests.clone()
    }

    /// All ACCEPTED `POST /api/v1/events` requests (deduped + rejected ingests
    /// are NOT recorded here — so the count is the CloudEvents OpenMeter counted).
    fn ingested_events(&self) -> Vec<RecordedRequest> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == "POST" && r.path.starts_with("/api/v1/events"))
            .collect()
    }

    /// Turn OFF the `id` dedup window to simulate a >24h re-drive (the C2
    /// scenario): a repeated id is no longer dropped — OpenMeter would SUM it
    /// again. Only the caller's reconcile-against-aggregate can then prevent a
    /// double-count.
    fn disable_dedupe(&self) {
        self.state.lock().unwrap().dedupe_enabled = false;
    }

    /// Reject EVERY subsequent ingest with a 400 (a permanently mis-provisioned
    /// meter) — drives the M2 durable-failure test.
    fn reject_all_ingest(&self) {
        self.state.lock().unwrap().reject_ingest = true;
    }

    /// Allow ingests again (recovery path for the M2 reset assertion).
    fn allow_ingest(&self) {
        self.state.lock().unwrap().reject_ingest = false;
    }

    /// The meter's aggregated value OpenMeter has accepted for `subject` (the SUM
    /// the `/query` endpoint serves). The C2 reconcile reads this.
    fn aggregate_for(&self, subject: &str) -> u64 {
        self.state
            .lock()
            .unwrap()
            .accepted_value_by_subject
            .get(subject)
            .copied()
            .unwrap_or(0)
    }
}

async fn start_mock_openmeter() -> MockOpenMeter {
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

    MockOpenMeter { state, base_url }
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
    let mut content_type = None;
    let mut authorization = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_string();
            match key.as_str() {
                "content-length" => content_length = val.parse().unwrap_or(0),
                "content-type" => content_type = Some(val),
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
            content_type,
            authorization,
            body,
        },
        body_start + content_length,
    ))
}

/// Answer an OpenMeter CloudEvent ingest with a 204-shaped JSON 200, recording
/// every ACCEPTED request. Faithful OpenMeter semantics:
///   * dedup window — a repeated `id` is dropped and NOT re-counted while
///     `dedupe_enabled`; with dedup OFF (a >24h re-drive) the repeat is SUMMED
///     again (the C2 hazard the caller's reconcile must defuse);
///   * aggregate — an ACCEPTED (non-deduped) event's `data.value` is added to the
///     per-subject running total served by `/query`.
/// Also serves `GET /api/v1/meters/{slug}/query` with that total.
fn handle_mock_request(req: &RecordedRequest, state: &Arc<Mutex<MockState>>) -> Vec<u8> {
    if req.method == "POST" && req.path.starts_with("/api/v1/events") {
        // Parse the CloudEvent JSON body.
        let json: serde_json::Value = match serde_json::from_str(&req.body) {
            Ok(v) => v,
            Err(_) => return http_400_json("invalid_json", "body is not valid CloudEvents JSON"),
        };
        let id = json.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let subject = json
            .get("subject")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let value: u64 = json
            .get("data")
            .and_then(|d| d.get("value"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);

        let mut st = state.lock().unwrap();
        if st.reject_ingest {
            // Simulate a permanent 4xx (mis-provisioned meter / subject): NOT
            // counted, NOT recorded — the push fails.
            return http_400_json("resource_missing", "no such meter for this event");
        }
        if st.dedupe_enabled && st.seen_ids.contains(&id) {
            // Within the dedup window: drop, do NOT count again. OpenMeter returns
            // a 2xx for an accepted-but-deduped event.
            return http_204();
        }
        st.seen_ids.insert(id);
        // ACCEPTED → add to the per-subject aggregate the meter would report.
        *st.accepted_value_by_subject.entry(subject).or_insert(0) += value;
        st.requests.push(req.clone());
        return http_204();
    }
    if req.method == "GET" && req.path.contains("/query") {
        // Serve the meter's aggregate for the queried subject. Shape mirrors
        // OpenMeter's `/query`: a `data` list of value rows.
        let subject = query_param(&req.path, "subject").unwrap_or_default();
        let total = state
            .lock()
            .unwrap()
            .accepted_value_by_subject
            .get(&subject)
            .copied()
            .unwrap_or(0);
        state.lock().unwrap().requests.push(req.clone());
        let json = format!(r#"{{"data":[{{"value":{total},"subject":"{subject}"}}]}}"#);
        return http_200_json(&json);
    }
    state.lock().unwrap().requests.push(req.clone());
    http_200_json(r#"{"object":"unknown"}"#)
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

/// OpenMeter's ingest returns 204 No Content on accept. The client treats any
/// 2xx as success.
fn http_204() -> Vec<u8> {
    b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\nconnection: keep-alive\r\n\r\n".to_vec()
}

/// An error body with an HTTP 400 (what the client maps to a Transport error).
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

/// Extract a CloudEvent field from a recorded ingest body (the JSON the cyper
/// client put on the wire).
fn cloudevent_field<'a>(body: &'a str, path: &[&str]) -> Option<serde_json::Value> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    let mut cur = &json;
    for key in path {
        cur = cur.get(key)?;
    }
    Some(cur.clone())
}

// ===========================================================================
// Fixture (real PG + a real OpenMeterProvider pointed at the mock server).
// ===========================================================================

struct Fixture {
    state: Arc<AppState>,
    mock: MockOpenMeter,
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
    let mock = start_mock_openmeter().await;
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

    // THE faithful seam: a REAL OpenMeterProvider whose base URL is the mock
    // server. The export cron pushes through the real cyper client over the wire.
    let provider = build_provider(&MeteringProviderConfig::openmeter(OpenMeterConfig {
        base_url: mock.base_url.clone(),
        token: SecretString::new("om_test_mock".to_string()),
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
        mock,
        blob_root,
        deploy_tmp_dir,
    }
}

// ---------------------------------------------------------------------------
// DB seeding helpers (mirror metering_export_test).
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

async fn make_plan(state: &AppState) -> String {
    make_plan_with_included(state, 0).await
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
    let plan_id = format!("pln_omexp_{}", Uuid::new_v4().simple());
    let fx_one_cent: i64 = 1_000_000_000_000;
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'omexp-test', 0, $2, $3, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', 100000)",
            &[&plan_id, &included_units, &fx_one_cent],
        )
        .await
        .expect("seed priced plan");
    plan_id
}

async fn make_owned_app(state: &AppState, plan_id: &str, owner: Uuid) -> Uuid {
    let name = format!("omexp-{}", Uuid::new_v4());
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

/// The mock's wall-clock "now" (unix seconds).
fn mock_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A DISTINCT, isolated calendar-month period bucket keyed off `year_month`.
/// Each test uses its OWN month (a far-future one, distinct from the Stripe
/// suite's buckets) so the FLEET-WIDE export sweep sees only that test's app.
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

/// `report_usage` (via the export cron) pushes a well-formed CloudEvent with the
/// correct CU `data.value` + `subject` + deterministic `id`, stamped at `now` —
/// through the REAL cyper client hitting the mock-OpenMeter server.
#[compio::test]
async fn export_pushes_cu_as_cloudevent_with_correct_value_subject_and_id() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "push").await;
    let _export = EXPORT_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let period = month_period(2032, 1);

    let creator = make_user(&fx.state, "push").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    // OpenMeter keys the subject on the creator's customer handle (the cron's
    // existing per-creator resolution); seed one (the "OpenMeter + Native" shape).
    let cus = format!("cus_om_push_{}", Uuid::new_v4().simple());
    fx.state.stripe_store.set_customer(creator, &cus).await.unwrap();

    ingest_at(&fx.state, app, 750, period, 1).await; // 750 requests × 1 CU = 750 CU

    let now = mock_now();
    let n = metering_export::tick_at(&fx.state, period, now).await.expect("tick");
    assert_eq!(n, 1, "one app exported");

    let events = fx.mock.ingested_events();
    assert_eq!(events.len(), 1, "exactly one CloudEvent ingested");
    let ev = &events[0];
    // Bearer auth + the CloudEvents content type on the wire.
    assert_eq!(ev.authorization.as_deref(), Some("Bearer om_test_mock"), "bearer auth sent");
    assert_eq!(
        ev.content_type.as_deref(),
        Some("application/cloudevents+json"),
        "CloudEvents content type"
    );
    // The CloudEvent body: specversion 1.0, type, subject, data.value = CU.
    assert_eq!(
        cloudevent_field(&ev.body, &["specversion"]).and_then(|v| v.as_str().map(String::from)),
        Some("1.0".to_string()),
        "specversion 1.0; body={}", ev.body
    );
    assert_eq!(
        cloudevent_field(&ev.body, &["type"]).and_then(|v| v.as_str().map(String::from)),
        Some(OM_EVENT_TYPE.to_string()),
        "type == event type; body={}", ev.body
    );
    assert_eq!(
        cloudevent_field(&ev.body, &["subject"]).and_then(|v| v.as_str().map(String::from)),
        Some(cus.clone()),
        "subject == creator handle; body={}", ev.body
    );
    assert_eq!(
        cloudevent_field(&ev.body, &["data", "value"]).and_then(|v| v.as_u64()),
        Some(750),
        "data.value == 750 CU; body={}", ev.body
    );
    // The push is per CUSTOMER now — `data` carries NO per-app id.
    assert!(
        cloudevent_field(&ev.body, &["data", "app_id"]).is_none(),
        "data carries no app_id on the per-creator export grain; body={}", ev.body
    );
    // The deterministic id is the (creator, period, 0→750) window identifier.
    let expected_id = metering_export::export_identifier(&creator, period, 0, 750);
    assert_eq!(
        cloudevent_field(&ev.body, &["id"]).and_then(|v| v.as_str().map(String::from)),
        Some(expected_id),
        "CloudEvent id == deterministic identifier; body={}", ev.body
    );
    // The `time` is the consumption instant (within Stripe-equivalent window
    // [now−35d, now+5min]); we assert it's near `now` and parses RFC3339.
    let time_str = cloudevent_field(&ev.body, &["time"])
        .and_then(|v| v.as_str().map(String::from))
        .expect("time on the wire");
    let parsed = chrono::DateTime::parse_from_rfc3339(&time_str).expect("RFC3339 time");
    let ts = parsed.timestamp();
    assert!(
        (ts - now).abs() <= 5 * 60,
        "CloudEvent time {ts} ≈ now {now} (the consumption instant, not the future period.end)"
    );

    // OpenMeter's aggregate reflects the accepted CU; the high-water advanced.
    assert_eq!(fx.mock.aggregate_for(&cus), 750, "OpenMeter counted 750 CU");
    let hw = read_high_water(&fx.state, &creator, period).await;
    assert_eq!(hw, Some(750), "exported_units high-water == cumulative CU");
}

/// `reported_total` reads OpenMeter's aggregate — driving the cron's delta. Two
/// ticks: tick 1 exports N, usage grows to M, tick 2 exports M−N (not M);
/// OpenMeter SUMS to M. Proves delta export + that `meter_query` feeds the cron.
#[compio::test]
async fn export_computes_delta_via_openmeter_aggregate_across_two_ticks() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "delta").await;
    let _export = EXPORT_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let period = month_period(2032, 2);

    let creator = make_user(&fx.state, "delta").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    let cus = format!("cus_om_delta_{}", Uuid::new_v4().simple());
    fx.state.stripe_store.set_customer(creator, &cus).await.unwrap();

    ingest_at(&fx.state, app, 100, period, 1).await;
    let n1 = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("tick 1");
    assert_eq!(n1, 1);

    ingest_at(&fx.state, app, 150, period, 2).await; // grows to 250

    let n2 = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("tick 2");
    assert_eq!(n2, 1, "the second tick pushes the (non-zero) delta");

    let events = fx.mock.ingested_events();
    assert_eq!(events.len(), 2, "two CloudEvents: the initial export + the delta");
    assert_eq!(
        cloudevent_field(&events[0].body, &["data", "value"]).and_then(|v| v.as_u64()),
        Some(100),
        "tick 1 pushed 100"
    );
    assert_eq!(
        cloudevent_field(&events[1].body, &["data", "value"]).and_then(|v| v.as_u64()),
        Some(150),
        "tick 2 pushed the DELTA 150, not the cumulative 250"
    );
    // OpenMeter's aggregate SUM == cumulative 250.
    assert_eq!(fx.mock.aggregate_for(&cus), 250, "OpenMeter SUM == 250");
    assert_eq!(read_high_water(&fx.state, &creator, period).await, Some(250));
}

/// Second tick with NO new usage is a pure no-op (delta 0 ⇒ no CloudEvent).
#[compio::test]
async fn second_export_tick_with_no_new_usage_is_a_noop() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "noop").await;
    let _export = EXPORT_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let period = month_period(2032, 3);

    let creator = make_user(&fx.state, "noop").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, &format!("cus_om_noop_{}", Uuid::new_v4().simple())).await.unwrap();
    ingest_at(&fx.state, app, 300, period, 1).await;

    let n1 = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("tick 1");
    assert_eq!(n1, 1);
    assert_eq!(fx.mock.ingested_events().len(), 1);

    let n2 = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("tick 2");
    assert_eq!(n2, 0, "second tick is a no-op (high-water == cumulative ⇒ delta 0)");
    assert_eq!(fx.mock.ingested_events().len(), 1, "no double-push");
}

// ===========================================================================
// C2 (inherited) — >24h re-drive past the dedup window reconciles against
// OpenMeter's aggregate ⇒ NO double-count.
// ===========================================================================

/// C2 (RED→GREEN): a crash AFTER the OpenMeter push but BEFORE the high-water
/// UPDATE leaves the local high-water stale. Re-driven PAST OpenMeter's dedup
/// window (mock dedupe DISABLED), a blind re-push of `current − high_water` would
/// be SUMMED twice (double-count). The cron reconciles against OpenMeter's
/// AGGREGATE (via the provider's `reported_total` → `meter_query`):
/// `delta = current − max(high_water, aggregate)` ⇒ the re-drive pushes nothing.
///
/// RED proof: if the cron skipped the `reported_total` reconcile and pushed
/// `current − high_water`, with dedup OFF OpenMeter's aggregate would land at 800
/// — the double-count this asserts against. (The shared, hardened cron already
/// does the reconcile; this test proves it holds when fed OpenMeter's aggregate.)
#[compio::test]
async fn redrive_past_dedupe_window_does_not_double_count() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "c2redrive").await;
    let _export = EXPORT_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let period = month_period(2032, 4);

    let creator = make_user(&fx.state, "c2").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    let cus = format!("cus_om_c2_{}", Uuid::new_v4().simple());
    fx.state.stripe_store.set_customer(creator, &cus).await.unwrap();

    ingest_at(&fx.state, app, 400, period, 1).await;

    // First sweep: pushes 400, lands in OpenMeter's aggregate, advances high-water.
    let n1 = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("tick 1");
    assert_eq!(n1, 1);
    assert_eq!(fx.mock.aggregate_for(&cus), 400, "OpenMeter counted 400 after tick 1");
    assert_eq!(read_high_water(&fx.state, &creator, period).await, Some(400));

    // Simulate the crash window: roll the high-water BACK to 0 (as if the push had
    // landed at OpenMeter but the high-water UPDATE never committed). OpenMeter's
    // aggregate still holds 400.
    fx.state
        .control_pg
        .execute(
            "UPDATE zeroship.metering_exports SET exported_units = 0 WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .expect("roll back high-water to simulate crash-before-update");

    // >24h later: OpenMeter's id dedup window has expired.
    fx.mock.disable_dedupe();

    // Re-drive. WITHOUT the aggregate reconcile this would re-push 400 and (dedup
    // OFF) OpenMeter would SUM to 800. WITH the reconcile (reported_total → 400)
    // the cron pushes 400 − max(0, 400) = 0.
    let n2 = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("tick 2");

    assert_eq!(
        fx.mock.aggregate_for(&cus),
        400,
        "OpenMeter's aggregate stays 400 — the >24h re-drive did NOT double-count (reconciled against the aggregate)"
    );
    assert_eq!(n2, 0, "the re-drive pushed nothing (the aggregate already covered current)");
    assert_eq!(read_high_water(&fx.state, &creator, period).await, Some(400), "high-water self-healed");
}

// ===========================================================================
// M1 (inherited) — push BILLABLE CU (gross − included).
// ===========================================================================

/// M1 (RED→GREEN): for a plan with a NON-ZERO `included_units`, the export pushes
/// BILLABLE CU (`gross − included`), the SAME quantity the spend cap treats as
/// billable — NOT the gross `total_units`. (The shared cron computes billable CU;
/// this proves it flows through the OpenMeter CloudEvent.)
#[compio::test]
async fn export_pushes_billable_cu_honoring_included_units() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "m1parity").await;
    let _export = EXPORT_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let period = month_period(2032, 5);

    let creator = make_user(&fx.state, "m1").await;
    let included: i64 = 200; // gross 750 ⇒ billable 550
    let plan = make_plan_with_included(&fx.state, included).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    let cus = format!("cus_om_m1_{}", Uuid::new_v4().simple());
    fx.state.stripe_store.set_customer(creator, &cus).await.unwrap();

    ingest_at(&fx.state, app, 750, period, 1).await;

    let n = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("tick");
    assert_eq!(n, 1);

    let events = fx.mock.ingested_events();
    assert_eq!(events.len(), 1);
    assert_eq!(
        cloudevent_field(&events[0].body, &["data", "value"]).and_then(|v| v.as_u64()),
        Some(550),
        "pushed BILLABLE CU 550 (gross 750 − included 200), NOT gross; body={}",
        events[0].body
    );
    assert_eq!(fx.mock.aggregate_for(&cus), 550, "OpenMeter counted the BILLABLE 550");
    assert_eq!(read_high_water(&fx.state, &creator, period).await, Some(550));
}

// ===========================================================================
// HIGH-severity multi-app revenue-loss fix — per-creator aggregation (symmetric
// with the Stripe rail; OpenMeter aggregates per subject = creator handle).
// ===========================================================================

/// THE HIGH-severity bug (RED→GREEN), OpenMeter rail. A creator owning TWO metered
/// apps, both with current-period usage. OpenMeter sums per `subject` (the creator
/// handle), so the export must reconcile + push at the CREATOR grain: the subject
/// aggregate (and the cumulative pushed total) must equal the SUM of BOTH apps'
/// billable CU.
///
/// PRE-FIX (per-app export reconciled against the per-subject aggregate): app A
/// pushes → the subject aggregate covers it → app B reconciles to `delta = 0` →
/// app B's CU never bills and its high-water silently advances. The aggregate would
/// land at ONLY app A's CU (300), not 300+450=750. This asserts 750 — FAILS pre-fix
/// (would see 300), PASSES post-fix. A re-drive is idempotent (no double-push).
#[compio::test]
async fn two_apps_one_creator_bills_the_sum_of_both_apps_cu() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "multiapp").await;
    let _export = EXPORT_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let period = month_period(2032, 7); // distinct isolated bucket

    let creator = make_user(&fx.state, "multiapp").await;
    let plan = make_plan(&fx.state).await; // 0 included; 1 CU/req
    let app_a = make_owned_app(&fx.state, &plan, creator).await;
    let app_b = make_owned_app(&fx.state, &plan, creator).await;
    let cus = format!("cus_om_multiapp_{}", Uuid::new_v4().simple());
    fx.state.stripe_store.set_customer(creator, &cus).await.unwrap();

    // App A: 300 CU. App B: 450 CU. Creator billable = 300 + 450 = 750.
    ingest_at(&fx.state, app_a, 300, period, 1).await;
    ingest_at(&fx.state, app_b, 450, period, 2).await;

    let n = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("tick");
    assert_eq!(n, 1, "ONE creator exported (a single subject-level delta, not one-per-app)");

    // THE core assertion: the subject aggregate == BOTH apps' CU summed. Pre-fix
    // this is 300 (app B silently zeroed); post-fix it is 750.
    assert_eq!(
        fx.mock.aggregate_for(&cus),
        750,
        "the subject aggregate == Σ both apps' billable CU (300 + 450) — app B is NOT silently zeroed"
    );

    // Exactly one CloudEvent (per creator), carrying the summed CU under the subject.
    let events = fx.mock.ingested_events();
    assert_eq!(events.len(), 1, "exactly one creator-level CloudEvent");
    assert_eq!(
        cloudevent_field(&events[0].body, &["data", "value"]).and_then(|v| v.as_u64()),
        Some(750),
        "the single push carried the creator's SUMMED billable CU 750; body={}", events[0].body
    );
    assert_eq!(
        cloudevent_field(&events[0].body, &["subject"]).and_then(|v| v.as_str().map(String::from)),
        Some(cus.clone()),
        "pushed under the creator's subject handle; body={}", events[0].body
    );

    // The high-water is CREATOR-keyed and equals the summed billable CU.
    assert_eq!(
        read_high_water(&fx.state, &creator, period).await,
        Some(750),
        "creator high-water == Σ both apps' billable CU"
    );

    // Idempotent re-drive: a second tick with no new usage pushes nothing.
    let n2 = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("re-drive tick");
    assert_eq!(n2, 0, "the re-drive is a no-op (creator high-water already covers current)");
    assert_eq!(fx.mock.ingested_events().len(), 1, "no double-push across the re-drive");
    assert_eq!(fx.mock.aggregate_for(&cus), 750, "aggregate unchanged after the idempotent re-drive");
}

/// THE per-app-included-quota bug (RED→GREEN), OpenMeter rail — symmetric with
/// the Stripe test. A creator owning TWO apps on DIFFERENT plans with DIFFERENT
/// non-zero `included_units`, where ONE app is UNDER its quota. The included
/// subtraction is PER APP, so the creator's billable CU must be `Σ_app max(0,
/// gross_app − included_app)`, NOT `max(0, Σgross − Σincluded)`.
///
///   * App A: gross 100, included 500 ⇒ per-app billable 0 (under quota).
///   * App B: gross 900, included 100 ⇒ per-app billable 800.
///   * Authoritative Σ per-app billable = 800 (what the spend cap bills).
///
/// PRE-FIX (`current = max(0, Σgross − Σincluded)` = `max(0, 1000−600)` = 400):
/// App A's unused 400 units of included quota silently offset App B's overage ⇒
/// under-bills by 400. This asserts 800 — FAILS pre-fix (sees 400), PASSES once
/// the included subtraction floors per app via `pricing::billable_units`.
#[compio::test]
async fn two_apps_different_quotas_floor_included_per_app() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "mixedquota").await;
    let _export = EXPORT_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let period = month_period(2032, 8); // distinct isolated bucket

    let creator = make_user(&fx.state, "mixedquota").await;
    let plan_a = make_plan_with_included(&fx.state, 500).await;
    let plan_b = make_plan_with_included(&fx.state, 100).await;
    let app_a = make_owned_app(&fx.state, &plan_a, creator).await;
    let app_b = make_owned_app(&fx.state, &plan_b, creator).await;
    let cus = format!("cus_om_mixedquota_{}", Uuid::new_v4().simple());
    fx.state.stripe_store.set_customer(creator, &cus).await.unwrap();

    // App A: gross 100, included 500 ⇒ billable 0 (UNDER quota).
    // App B: gross 900, included 100 ⇒ billable 800.
    ingest_at(&fx.state, app_a, 100, period, 1).await;
    ingest_at(&fx.state, app_b, 900, period, 2).await;

    let n = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("tick");
    assert_eq!(n, 1, "ONE creator exported (a single subject-level delta)");

    // THE core assertion: per-app floor then sum = 800. Per-creator subtraction
    // (the bug) would push 400 (App A's unused quota offsetting App B's overage).
    assert_eq!(
        fx.mock.aggregate_for(&cus),
        800,
        "the subject aggregate == Σ per-app max(0, gross−included) = max(0,100−500) + \
         max(0,900−100) = 0 + 800 = 800 — NOT the per-creator-subtraction 400"
    );

    // Exactly one CloudEvent carrying the per-app-floored summed CU.
    let events = fx.mock.ingested_events();
    assert_eq!(events.len(), 1, "exactly one creator-level CloudEvent");
    assert_eq!(
        cloudevent_field(&events[0].body, &["data", "value"]).and_then(|v| v.as_u64()),
        Some(800),
        "the single push carried 800 (per-app floor), not 400; body={}", events[0].body
    );

    // The CREATOR-keyed high-water == the per-app-floored sum.
    assert_eq!(
        read_high_water(&fx.state, &creator, period).await,
        Some(800),
        "creator high-water == Σ per-app billable CU (800), not 400"
    );

    // Parity with the spend cap: Σ per-app `pricing::billable_units` == 800.
    let metering = Metering::new(fx.state.registry.clone());
    let weights = zeroship_control::pricing_store::PricingStore::new(fx.state.registry.clone())
        .weights()
        .await
        .expect("weights");
    let totals_a = metering.period_totals(&app_a, period).await.expect("totals A");
    let totals_b = metering.period_totals(&app_b, period).await.expect("totals B");
    let price_a = zeroship_control::pricing::PlanPrice {
        included_units: 500,
        ..Default::default()
    };
    let price_b = zeroship_control::pricing::PlanPrice {
        included_units: 100,
        ..Default::default()
    };
    let bill_a = zeroship_control::pricing::billable_units(&price_a, &totals_a, &weights)
        .expect("billable A");
    let bill_b = zeroship_control::pricing::billable_units(&price_b, &totals_b, &weights)
        .expect("billable B");
    assert_eq!(bill_a, 0, "App A under quota ⇒ per-app billable 0");
    assert_eq!(bill_b, 800, "App B over quota ⇒ per-app billable 800");
    assert_eq!(bill_a + bill_b, 800, "spend-cap-parity: Σ per-app billable_units == 800");

    // Idempotent re-drive.
    let n2 = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("re-drive tick");
    assert_eq!(n2, 0, "the re-drive is a no-op (creator high-water already covers current)");
    assert_eq!(fx.mock.ingested_events().len(), 1, "no double-push across the re-drive");
    assert_eq!(fx.mock.aggregate_for(&cus), 800, "aggregate unchanged after the idempotent re-drive");
}

// ===========================================================================
// M2 (inherited) — durable per-creator export failure surface for OpenMeter.
// ===========================================================================

/// M2 (RED→GREEN): a failing CloudEvent ingest must be DURABLE + queryable. The
/// mock rejects every ingest (a permanent 4xx); the sweep records the failure
/// surface (`consecutive_failures`, `last_error`); a later SUCCESS resets it.
#[compio::test]
async fn export_failure_is_recorded_durably() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "m2fail").await;
    let _export = EXPORT_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let period = month_period(2032, 6);

    let creator = make_user(&fx.state, "m2").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, &format!("cus_om_m2_{}", Uuid::new_v4().simple())).await.unwrap();
    ingest_at(&fx.state, app, 300, period, 1).await;

    fx.mock.reject_all_ingest();

    let n = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("tick");
    assert_eq!(n, 0, "nothing exported (the push failed)");

    let (failures, last_error) = read_failure_state(&fx.state, &creator, period).await;
    assert_eq!(failures, Some(1), "consecutive_failures bumped to 1 on the failed push");
    assert!(
        last_error.as_deref().is_some_and(|e| e.contains("report_usage")),
        "last_error records the failure (got {last_error:?})"
    );
    assert_eq!(read_high_water(&fx.state, &creator, period).await, Some(0));

    let _ = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("tick 2");
    let (failures2, _) = read_failure_state(&fx.state, &creator, period).await;
    assert_eq!(failures2, Some(2), "a second failure bumps consecutive_failures to 2");

    fx.mock.allow_ingest();
    let n3 = metering_export::tick_at(&fx.state, period, mock_now()).await.expect("tick 3");
    assert_eq!(n3, 1, "the recovered push exports");
    let (failures3, last_error3) = read_failure_state(&fx.state, &creator, period).await;
    assert_eq!(failures3, Some(0), "a successful export RESETS consecutive_failures to 0");
    assert_eq!(last_error3, None, "last_error cleared on success");
}

/// The first-of-month `billing_period` DATE for a unix-seconds period start.
fn period_d(period_start: i64) -> chrono::NaiveDate {
    use chrono::{Datelike, TimeZone};
    let dt = chrono::Utc.timestamp_opt(period_start, 0).single().unwrap();
    chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), 1).unwrap()
}

/// Read the `metering_exports` high-water for a `(creator, period)`.
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

/// Read the M2 durable failure surface for a `(creator, period)`.
async fn read_failure_state(
    state: &AppState,
    creator: &Uuid,
    period: i64,
) -> (Option<i32>, Option<String>) {
    let rows = state
        .control_pg
        .query(
            "SELECT consecutive_failures, last_error FROM zeroship.metering_exports \
             WHERE creator_id = $1 AND period = $2::date",
            &[creator, &period_d(period)],
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
