//! Integration tests for the billing-reconcile cron + Stripe client (billing
//! PR6, ISS-31, Stream-1).
//!
//! FAITHFUL by construction: the tests drive the REAL `cyper`-based
//! [`StripeClient`] against a localhost **mock-Stripe HTTP server** (a small
//! HTTP/1.1 server stood up in-test on `compio::net::TcpListener` that speaks
//! Stripe's form/JSON protocol and RECORDS every request). There is NO stubbed
//! client object on the wire path: the reconciler builds a real `StripeClient`
//! pointed at the mock via `AppState.stripe_base_url`, so the form encoding,
//! the `Authorization: Bearer` + `Idempotency-Key` headers, the HTTP round-trip
//! and the JSON parse are all exercised end to end.
//!
//! Real Postgres via `CONTROL_TEST_DB`; silent skip otherwise. The DB must have
//! changeset 0040 applied.

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ntex::http::StatusCode;
use ntex::web::{self, test};

use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use uuid::Uuid;

use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::cron::billing_reconcile;
use zeroship_control::metering::Metering;
use zeroship_control::stripe_client::{Period, StripeApi, StripeClient};
use zeroship_control::{
    AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_core::types::{AppUsage, UsageReport};

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn tmpdir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("zs-bill-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&p).expect("mk tmpdir");
    p
}

// ===========================================================================
// Mock-Stripe HTTP server — a real localhost server the REAL cyper client hits.
// ===========================================================================

/// One recorded inbound HTTP request to the mock-Stripe server.
#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    idempotency_key: Option<String>,
    authorization: Option<String>,
    body: String,
    /// True if the mock served this request from its Idempotency-Key replay
    /// cache (i.e. Stripe would NOT have created a new object).
    replayed: bool,
}

#[derive(Default)]
struct MockState {
    requests: Vec<RecordedRequest>,
    /// Idempotency-Key → the exact JSON response previously returned for that
    /// key. Real Stripe replays the ORIGINAL response on a repeated key (within
    /// its 24h window); a faithful mock must do the same so the idempotency
    /// tests actually exercise layer-2 (the deterministic Stripe key). See
    /// `dedupe_by_key`.
    idempotency_replies: HashMap<String, String>,
    /// When false, the mock does NOT replay by Idempotency-Key — it treats every
    /// request as fresh. This simulates Stripe's key window having EXPIRED
    /// (>24h), proving the per-app LEDGER (not Stripe's key) is what guarantees
    /// at-most-once posting (CRIT-1).
    dedupe_by_key: bool,
    /// Pending invoice items, in creation order: (id, customer, zs_item_key).
    /// A faithful Stripe lists these on `GET /v1/invoiceitems?...&pending=true`
    /// so the reconciler's `find_invoice_item_by_key` (C1) can adopt an
    /// already-posted item on a >24h re-drive instead of double-posting. An item
    /// swept onto a finalized invoice would drop off `pending=true`, but our
    /// finalize never sweeps a SECOND copy, so leaving them is faithful enough
    /// for the >24h adopt path under test.
    invoice_items: Vec<(String, String, Option<String>)>,
}

#[derive(Clone)]
struct MockStripe {
    state: Arc<Mutex<MockState>>,
    base_url: String,
}

impl MockStripe {
    fn requests(&self) -> Vec<RecordedRequest> {
        self.state.lock().unwrap().requests.clone()
    }

    fn count_path(&self, method: &str, path_prefix: &str) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.method == method && r.path.starts_with(path_prefix))
            .count()
    }

    /// Count distinct POSTs to `path_prefix` that ACTUALLY created a new object
    /// (i.e. were not replayed from a prior identical Idempotency-Key). This is
    /// the count that matters for "double-bill": even if a request was sent
    /// twice, a deduped reply means Stripe created the object once.
    fn count_created(&self, method: &str, path_prefix: &str) -> usize {
        let st = self.state.lock().unwrap();
        st.requests
            .iter()
            .filter(|r| r.method == method && r.path.starts_with(path_prefix) && !r.replayed)
            .count()
    }

    /// Count POSTs to the EXACT path that actually created a new object (not
    /// replayed). Distinct from `count_created`, which matches by prefix — needed
    /// to separate `POST /v1/invoices` (draft create) from
    /// `POST /v1/invoices/{id}/finalize` (which shares the `/v1/invoices` prefix).
    fn count_created_exact(&self, method: &str, path: &str) -> usize {
        let st = self.state.lock().unwrap();
        st.requests
            .iter()
            .filter(|r| r.method == method && r.path == path && !r.replayed)
            .count()
    }

    /// Turn OFF Idempotency-Key replay to simulate Stripe's >24h key expiry.
    fn disable_dedupe(&self) {
        self.state.lock().unwrap().dedupe_by_key = false;
    }
}

/// Stand up a localhost HTTP/1.1 server that answers Stripe's create endpoints
/// with minimal Stripe-shaped JSON, recording every request. Each accepted
/// connection is served in its own detached task with a keep-alive loop (cyper
/// reuses connections). Returns a handle exposing the base URL + recorded calls.
async fn start_mock_stripe() -> MockStripe {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().expect("local_addr");
    let base_url = format!("http://{addr}");
    let state = Arc::new(Mutex::new(MockState {
        dedupe_by_key: true, // faithful default: replay by Idempotency-Key like real Stripe
        ..MockState::default()
    }));
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

    MockStripe { state, base_url }
}

/// Serve a single connection: read request(s), record them, respond. Loops to
/// honor HTTP keep-alive so a reused cyper connection's later requests are also
/// answered and recorded.
async fn serve_conn(mut stream: TcpStream, state: Arc<Mutex<MockState>>) {
    let mut acc: Vec<u8> = Vec::new();
    loop {
        // Parse as many complete requests as `acc` holds, draining each.
        loop {
            let Some((req, consumed)) = try_parse_request(&acc) else { break };
            acc.drain(0..consumed);
            let response = handle_mock_request(&req, &state);
            if stream.write_all(response).await.0.is_err() {
                return;
            }
        }
        // Need more bytes.
        let buf = vec![0u8; 4096];
        let compio::BufResult(n, buf) = stream.read(buf).await;
        match n {
            Ok(0) | Err(_) => return, // EOF or error → close
            Ok(read) => acc.extend_from_slice(&buf[..read]),
        }
    }
}

/// Attempt to parse ONE complete HTTP request from `buf`. Returns the parsed
/// request + the number of bytes consumed, or `None` if `buf` is incomplete.
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
        return None; // body not fully arrived
    }
    let body = String::from_utf8_lossy(&buf[body_start..body_start + content_length]).to_string();

    Some((
        RecordedRequest {
            method,
            path,
            idempotency_key,
            authorization,
            body,
            replayed: false,
        },
        body_start + content_length,
    ))
}

/// Produce a Stripe-shaped JSON 200 response for a recorded request and record
/// the request. The `id` returned is derived from the path so each endpoint
/// yields a plausible object id.
fn handle_mock_request(req: &RecordedRequest, state: &Arc<Mutex<MockState>>) -> Vec<u8> {
    // Faithful Stripe idempotency: if dedupe is on and we've seen this
    // Idempotency-Key before, replay the EXACT original response (Stripe does
    // not create a second object). Mark the recorded request `replayed` so the
    // test can distinguish "sent twice but deduped" from "created twice".
    {
        let mut st = state.lock().unwrap();
        if st.dedupe_by_key {
            if let Some(key) = req.idempotency_key.clone() {
                if let Some(prev) = st.idempotency_replies.get(&key).cloned() {
                    let mut rec = req.clone();
                    rec.replayed = true;
                    st.requests.push(rec);
                    return http_200_json(&prev);
                }
            }
        }
    }

    // GET /v1/invoiceitems?...&pending=true — list the pending items for a
    // customer (C1's `find_invoice_item_by_key`). Faithful Stripe list shape:
    // `{ "object":"list", "data":[ {id, metadata:{zs_item_key}}, … ] }`.
    if req.method == "GET" && req.path.starts_with("/v1/invoiceitems") {
        let customer = query_param(&req.path, "customer");
        let st = state.lock().unwrap();
        let data: Vec<String> = st
            .invoice_items
            .iter()
            .filter(|(_, cust, _)| customer.as_deref() == Some(cust.as_str()))
            .map(|(id, _, key)| match key {
                Some(k) => format!(
                    r#"{{"id":"{id}","object":"invoiceitem","metadata":{{"zs_item_key":"{k}"}}}}"#
                ),
                None => format!(r#"{{"id":"{id}","object":"invoiceitem","metadata":{{}}}}"#),
            })
            .collect();
        drop(st);
        let body = format!(r#"{{"object":"list","data":[{}]}}"#, data.join(","));
        state.lock().unwrap().requests.push(req.clone());
        return http_200_json(&body);
    }

    let new_item_id = format!("ii_mock_{}", short());
    let json: String = if req.path.starts_with("/v1/customers") {
        format!(r#"{{"id":"cus_mock_{}","object":"customer"}}"#, short())
    } else if req.path.starts_with("/v1/checkout/sessions") {
        format!(
            r#"{{"id":"cs_mock_{0}","object":"checkout.session","url":"https://checkout.stripe.test/cs_mock_{0}"}}"#,
            short()
        )
    } else if req.path.starts_with("/v1/invoiceitems") {
        format!(r#"{{"id":"{new_item_id}","object":"invoiceitem"}}"#)
    } else if req.path.contains("/finalize") {
        format!(r#"{{"id":"in_mock_final_{}","object":"invoice","status":"open"}}"#, short())
    } else if req.path.starts_with("/v1/invoices") {
        format!(r#"{{"id":"in_mock_{}","object":"invoice","status":"draft"}}"#, short())
    } else {
        r#"{"id":"obj_mock","object":"unknown"}"#.to_string()
    };

    {
        let mut st = state.lock().unwrap();
        // Record a created invoice item so the GET-list (adopt) path can find it.
        if req.method == "POST" && req.path.starts_with("/v1/invoiceitems") {
            let customer = form_param(&req.body, "customer").unwrap_or_default();
            let key = form_param(&req.body, "metadata[zs_item_key]");
            st.invoice_items.push((new_item_id.clone(), customer, key));
        }
        st.requests.push(req.clone());
        if let Some(key) = req.idempotency_key.clone() {
            st.idempotency_replies.entry(key).or_insert_with(|| json.clone());
        }
    }

    http_200_json(&json)
}

/// Extract a query-string parameter from a request path (e.g. `customer`).
fn query_param(path: &str, name: &str) -> Option<String> {
    let q = path.split_once('?').map(|(_, q)| q)?;
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == name {
                return Some(percent_decode(v));
            }
        }
    }
    None
}

/// Extract a form field from an `application/x-www-form-urlencoded` body. Keys
/// are matched after percent-decoding so `metadata[zs_item_key]` matches the
/// wire's `metadata%5Bzs_item_key%5D`.
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

/// Minimal `application/x-www-form-urlencoded` / query decode: `+` → space,
/// `%XX` → byte. Sufficient for the mock's keys/values under test.
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

/// Build a `200 OK` HTTP/1.1 response with a JSON body.
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
// Fixture (real PG + the mock-Stripe base URL wired into AppState).
// ===========================================================================

struct Fixture {
    state: Arc<AppState>,
    mock: MockStripe,
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
    let mock = start_mock_stripe().await;
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
        metering_provider: zeroship_control::metering::provider::build_provider(
            &zeroship_control::metering::provider::MeteringProviderConfig::native(),
        )
        .expect("native provider builds"),
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
// DB seeding helpers.
// ---------------------------------------------------------------------------

/// Insert a user (the creator). Returns its id.
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

/// Seed a plan that charges 1 cent/request with no included CU. CU pricing:
/// global weight `requests` = 1 CU/op × fx 10^12 pico-cents/CU (= 1 cent/CU).
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
    let plan_id = format!("pln_bill_{}", Uuid::new_v4().simple());
    let fx_one_cent: i64 = 1_000_000_000_000;
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'bill-test', 0, 0, $2, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', 100000)",
            &[&plan_id, &fx_one_cent],
        )
        .await
        .expect("seed priced plan");
    plan_id
}

/// Create an app on `plan_id` owned by `owner`. Returns the app id.
async fn make_owned_app(state: &AppState, plan_id: &str, owner: Uuid) -> Uuid {
    let name = format!("bill-{}", Uuid::new_v4());
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

/// Ingest usage directly at a given period_start (the CLOSED period the
/// reconciler bills). Mirrors `Metering::ingest_at`.
async fn ingest_at(state: &AppState, app: Uuid, requests: u64, period_start: i64, seq: u64) {
    let metering = Metering::new(state.registry.clone());
    let worker = format!("w-{}", Uuid::new_v4());
    metering
        .ingest_at(&report(&worker, seq, app, requests), period_start)
        .await
        .expect("ingest usage");
}

/// `now` placed mid-current-month so the CLOSED period is the previous month.
fn now_for_closed_period() -> i64 {
    chrono::Utc::now().timestamp()
}

fn prev_period(now: i64) -> i64 {
    billing_reconcile::previous_period_start_unix(now)
}

// ===========================================================================
// Tests.
// ===========================================================================

/// The reconciler builds Stripe invoice items per owned app from REAL
/// aggregates, then creates + finalizes an invoice — all through the real cyper
/// client hitting the mock server. Records on `billing_runs`.
#[compio::test]
async fn reconcile_creates_invoice_items_per_app_from_real_aggregates() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "items").await;
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "items").await;
    let plan = make_plan(&fx.state).await;
    let app1 = make_owned_app(&fx.state, &plan, creator).await;
    let app2 = make_owned_app(&fx.state, &plan, creator).await;
    // Customer must exist (set lazily by billing/setup in prod; here directly).
    fx.state.stripe_store.set_customer(creator, "cus_test_items").await.unwrap();

    ingest_at(&fx.state, app1, 500, period, 1).await; // 500c
    ingest_at(&fx.state, app2, 250, period, 2).await; // 250c

    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("tick");
    assert_eq!(billed, 1, "one creator billed");

    // Two invoice-item creates (one per app) + one invoice create + one finalize.
    assert_eq!(fx.mock.count_path("POST", "/v1/invoiceitems"), 2, "one item per app");
    assert_eq!(fx.mock.count_path("POST", "/v1/invoices"), 2, "create + finalize (both POST /v1/invoices…)");

    // billing_runs records the finalized invoice + the summed amount (750c).
    let rows = fx
        .state
        .control_pg
        .query(
            "SELECT amount_cents, stripe_invoice_id FROM zeroship.billing_runs \
             WHERE creator_id = $1 AND period_start = to_timestamp($2::double precision)",
            &[&creator, &(period as f64)],
        )
        .await
        .expect("read billing_runs");
    assert_eq!(rows.len(), 1, "exactly one billing_runs row for the period");
    let amount: i64 = rows[0].get("amount_cents");
    assert_eq!(amount, 750, "summed charge across both apps");
    let invoice_id: Option<String> = rows[0].get("stripe_invoice_id");
    assert!(invoice_id.is_some(), "stripe_invoice_id recorded after finalize");
}

/// THE no-double-bill guarantee. Run the tick TWICE for the same (creator,
/// period). The second run is a pure no-op via the `billing_runs` PK conflict —
/// the mock server sees the invoice-item creates EXACTLY ONCE.
///
/// RED→GREEN: remove the `billing_runs` ON CONFLICT claim and the second run
/// re-bills (the mock sees 4 invoice-item creates, not 2). The blueprint's
/// idempotency guard is what makes this GREEN.
#[compio::test]
async fn reconcile_is_idempotent_per_period() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "idem").await;
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "idem").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, "cus_test_idem").await.unwrap();
    ingest_at(&fx.state, app, 300, period, 1).await; // 300c

    let billed1 = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("tick 1");
    assert_eq!(billed1, 1, "first run bills the creator");

    let items_after_first = fx.mock.count_path("POST", "/v1/invoiceitems");
    assert_eq!(items_after_first, 1, "one invoice item on the first run");

    // Second run for the SAME period — must be a no-op (already billed).
    let billed2 = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("tick 2");
    assert_eq!(billed2, 0, "second run is a no-op (per-period idempotency)");

    // The mock server saw the invoice-item create EXACTLY ONCE total.
    assert_eq!(
        fx.mock.count_path("POST", "/v1/invoiceitems"),
        1,
        "no double-bill: the invoice item was created exactly once across two runs",
    );

    // Still exactly one billing_runs row.
    let rows = fx
        .state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.billing_runs WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .expect("count runs");
    assert_eq!(rows[0].get::<_, i64>("n"), 1, "exactly one run row");
}

/// The REAL cyper client sends a non-empty `Idempotency-Key` + the
/// `Authorization: Bearer` header on every mutating call. Asserted on the mock
/// server's recorded requests — proving the wire path, not a stubbed client.
#[compio::test]
async fn stripe_client_uses_cyper_and_sends_idempotency_key() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "hdr").await;

    // Drive the REAL client directly against the mock.
    let client = StripeClient::new(SecretString::new("sk_test_mock".to_string()))
        .with_base_url(fx.mock.base_url.clone());
    let period = Period { start: 1_700_000_000, end: 1_702_000_000 };
    let item = client
        .create_invoice_item("cus_x", 1234, "usd", "infra", period, "billitem:k1", "billitem:k1")
        .await
        .expect("create invoice item");
    assert!(item.starts_with("ii_mock_"), "parsed the ii_ id from the mock JSON");
    let draft = client
        .create_invoice("cus_x", &Uuid::new_v4().to_string(), "billrun:k2")
        .await
        .expect("create draft");
    assert!(draft.starts_with("in_mock_"), "parsed the draft invoice id");
    let invoice = client.finalize_invoice(&draft).await.expect("finalize");
    assert!(invoice.starts_with("in_mock_final_"), "parsed the finalized invoice id");

    let reqs = fx.mock.requests();
    let item_req = reqs.iter().find(|r| r.method == "POST" && r.path.starts_with("/v1/invoiceitems")).expect("item req");
    assert_eq!(item_req.idempotency_key.as_deref(), Some("billitem:k1"), "item idempotency key sent");
    assert_eq!(item_req.authorization.as_deref(), Some("Bearer sk_test_mock"), "bearer auth sent");
    // Form body carries the bracketed period params (proves form encoding).
    assert!(item_req.body.contains("period%5Bstart%5D=1700000000"), "period[start] form-encoded; body={}", item_req.body);
    assert!(item_req.body.contains("amount=1234"), "amount in form body");
    // The deterministic lookup key is stamped into metadata for the >24h adopt path (C1).
    assert!(item_req.body.contains("metadata%5Bzs_item_key%5D=billitem%3Ak1"), "zs_item_key metadata sent; body={}", item_req.body);

    let invoice_create = reqs.iter().find(|r| r.path == "/v1/invoices").expect("invoice create");
    assert_eq!(invoice_create.idempotency_key.as_deref(), Some("billrun:k2"), "invoice idempotency key sent");
    let finalize = reqs.iter().find(|r| r.path.contains("/finalize")).expect("finalize");
    assert_eq!(finalize.idempotency_key.as_deref(), Some(format!("finalize:{draft}").as_str()), "finalize idempotency key keyed on draft id");
}

/// `billing/setup` ensures a Customer exists, and a SECOND setup reuses the same
/// `cus_…` (only one `POST /v1/customers` ever fires). Drives the real client
/// through the store + mock; asserts the store persisted one customer id.
#[compio::test]
async fn setup_session_creates_customer_once() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "setup").await;
    let creator = make_user(&fx.state, "setup").await;

    // First setup: no customer yet → create one + a setup session.
    let client = StripeClient::new(SecretString::new("sk_test_mock".to_string()))
        .with_base_url(fx.mock.base_url.clone());
    // Mirror the handler's ensure-then-session flow twice.
    for _ in 0..2 {
        let existing = fx.state.stripe_store.get_customer(creator).await.unwrap();
        let customer = match existing {
            Some(c) => c,
            None => {
                let cus = client.create_customer("c@example.test", &creator.to_string()).await.unwrap();
                fx.state.stripe_store.set_customer(creator, &cus).await.unwrap();
                cus
            }
        };
        let _url = client
            .create_checkout_setup_session(&customer, "https://ok", "https://cancel")
            .await
            .unwrap();
    }

    assert_eq!(
        fx.mock.count_path("POST", "/v1/customers"),
        1,
        "the customer is created exactly once across two setups (reuse on the second)",
    );
    assert_eq!(fx.mock.count_path("POST", "/v1/checkout/sessions"), 2, "a session per setup");
    let stored = fx.state.stripe_store.get_customer(creator).await.unwrap();
    assert!(stored.is_some(), "customer id persisted to creator_billing");
}

/// Two apps owned by the SAME user_id roll into ONE creator invoice spanning
/// both apps (proves the owner-join grouping). Apps with no owner row are
/// skipped (an unowned app gets no invoice).
#[compio::test]
async fn reconcile_groups_apps_by_owner_via_app_members() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "owner").await;
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "owner").await;
    let plan = make_plan(&fx.state).await;
    let owned_a = make_owned_app(&fx.state, &plan, creator).await;
    let owned_b = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, "cus_test_owner").await.unwrap();

    // An app with NO owner row — must be skipped (no billable creator).
    let unowned = {
        let name = format!("unowned-{}", Uuid::new_v4());
        let rows = fx
            .state
            .control_pg
            .query(
                "INSERT INTO zeroship.apps (name, plan_id, api_key, api_key_hash) \
                 VALUES ($1, $2, $3, '') RETURNING id",
                &[&name, &plan, &Uuid::new_v4().to_string()],
            )
            .await
            .expect("insert unowned app");
        rows[0].get::<_, Uuid>("id")
    };

    ingest_at(&fx.state, owned_a, 100, period, 1).await; // 100c
    ingest_at(&fx.state, owned_b, 200, period, 2).await; // 200c
    ingest_at(&fx.state, unowned, 999, period, 3).await; // would be 999c — must NOT bill

    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("tick");
    assert_eq!(billed, 1, "one creator billed (the unowned app is skipped)");

    // One invoice item per OWNED app (2), not 3.
    assert_eq!(fx.mock.count_path("POST", "/v1/invoiceitems"), 2, "two owned apps → two items");

    // The creator's run amount spans both owned apps (300c), excluding unowned.
    let rows = fx
        .state
        .control_pg
        .query(
            "SELECT amount_cents FROM zeroship.billing_runs \
             WHERE creator_id = $1 AND period_start = to_timestamp($2::double precision)",
            &[&creator, &(period as f64)],
        )
        .await
        .expect("read run");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, i64>("amount_cents"), 300, "owned apps summed; unowned excluded");
}

/// Commit-then-crash recovery: a `billing_runs` row pre-exists with
/// `stripe_invoice_id IS NULL` (the run was claimed but the process died before
/// Stripe responded). The next tick RE-DRIVES it — the Stripe call fires with
/// the SAME deterministic idempotency key and the row is completed.
#[compio::test]
async fn crashed_run_with_null_invoice_id_is_redriven() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "crash").await;
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "crash").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, "cus_test_crash").await.unwrap();
    ingest_at(&fx.state, app, 400, period, 1).await; // 400c

    // Simulate the crash window: the run row exists (claimed) but no invoice id.
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.billing_runs (creator_id, period_start, amount_cents) \
             VALUES ($1, to_timestamp($2::double precision), 400)",
            &[&creator, &(period as f64)],
        )
        .await
        .expect("pre-insert NULL-invoice run");

    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("tick");
    assert_eq!(billed, 1, "the NULL-invoice run is re-driven and completed");

    // The Stripe finalize used the deterministic invoice idempotency key.
    let invoice_create = fx
        .mock
        .requests()
        .into_iter()
        .find(|r| r.path == "/v1/invoices")
        .expect("invoice create fired");
    let expected_key = billing_reconcile::invoice_idempotency_key(&creator, period);
    assert_eq!(
        invoice_create.idempotency_key.as_deref(),
        Some(expected_key.as_str()),
        "re-drive replays the SAME deterministic invoice idempotency key",
    );

    // The row is now completed.
    let rows = fx
        .state
        .control_pg
        .query(
            "SELECT stripe_invoice_id FROM zeroship.billing_runs \
             WHERE creator_id = $1 AND period_start = to_timestamp($2::double precision)",
            &[&creator, &(period as f64)],
        )
        .await
        .expect("read run");
    assert!(rows[0].get::<_, Option<String>>("stripe_invoice_id").is_some(), "invoice id filled in");
}

/// MAJOR-2 (fail-closed) REGRESSION: weights present, a plan that INHERITS the
/// global FX (`fx_pico_cents_per_unit = NULL`), and the global `pricing_config`
/// default row REMOVED ⇒ the platform cannot price ⇒ the sweep must ABORT
/// (error out) and produce NO invoice and NO `billing_runs` row — never a silent
/// base-only $0 invoice (the revenue leak the critic flagged).
///
/// RED→GREEN: under the old `charge_cents` (fx None ⇒ 0 ⇒ base-only), this
/// creator with 600 requests would bill $0 silently and `tick_with` would return
/// `Ok`; here it returns `Err` and writes nothing.
#[compio::test]
async fn missing_default_fx_aborts_sweep_and_bills_no_one() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "nofx").await;
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "nofx").await;

    // Weights present (so usage WOULD accrue CU), but the plan inherits the FX
    // (NULL) and we delete the global default — leaving the FX unresolvable.
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units) \
             VALUES ('requests', 1, 1) \
             ON CONFLICT (metric) DO UPDATE SET units_per_op = 1, per_units = 1",
            &[],
        )
        .await
        .expect("seed weight");
    let plan_id = format!("pln_nofx_{}", Uuid::new_v4().simple());
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'nofx', 0, 0, NULL, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', 100000)",
            &[&plan_id],
        )
        .await
        .expect("seed inheriting plan");
    let app = make_owned_app(&fx.state, &plan_id, creator).await;
    fx.state.stripe_store.set_customer(creator, "cus_test_nofx").await.unwrap();
    ingest_at(&fx.state, app, 600, period, 1).await; // would be 600c IF priceable

    // Capture the shared singleton so we can RESTORE it before any assertion —
    // the pricing_config row is fleet-wide shared state across test binaries, so
    // a mid-test panic must NOT leak the deletion into a sibling test's pricing.
    let saved_fx: Option<i64> = fx
        .state
        .control_pg
        .query("SELECT fx_pico_cents_per_unit FROM zeroship.pricing_config WHERE id = 'global'", &[])
        .await
        .expect("read saved fx")
        .first()
        .map(|r| r.get("fx_pico_cents_per_unit"));

    // Remove the global default FX so the inheriting plan cannot resolve it.
    fx.state
        .control_pg
        .execute("DELETE FROM zeroship.pricing_config WHERE id = 'global'", &[])
        .await
        .expect("delete global pricing_config");

    // The sweep must FAIL CLOSED — abort with an error, not bill $0. Capture the
    // observations FIRST, then restore the singleton, then assert (so a failing
    // assert can never leak the deletion).
    let res = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now).await;
    let items = fx.mock.count_path("POST", "/v1/invoiceitems");
    let invoices = fx.mock.count_path("POST", "/v1/invoices");
    let runs = fx
        .state
        .control_pg
        .query(
            "SELECT amount_cents FROM zeroship.billing_runs \
             WHERE creator_id = $1 AND period_start = to_timestamp($2::double precision)",
            &[&creator, &(period as f64)],
        )
        .await
        .expect("read billing_runs");

    // Restore the shared singleton BEFORE asserting.
    let restore_fx = saved_fx.unwrap_or(30_000_000);
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.pricing_config (id, fx_pico_cents_per_unit) \
             VALUES ('global', $1) \
             ON CONFLICT (id) DO UPDATE SET fx_pico_cents_per_unit = EXCLUDED.fx_pico_cents_per_unit",
            &[&restore_fx],
        )
        .await
        .expect("restore global pricing_config");

    assert!(
        res.is_err(),
        "missing global default FX must abort the sweep (fail closed), not bill base-only $0"
    );
    assert_eq!(items, 0, "no item posted");
    assert_eq!(invoices, 0, "no invoice created");
    assert!(runs.is_empty(), "no billing_runs row — bill no one when the platform can't price");
}

/// Build the real `StripeClient` pointed at the fixture's mock — used by the
/// reconcile tests so the sweep drives the REAL cyper client (NOT a stub).
fn dummy_passthrough(fx: &Fixture) -> StripeClient {
    StripeClient::new(SecretString::new(
        fx.state.stripe_secret_key.expose_secret().to_string(),
    ))
    .with_base_url(fx.state.stripe_base_url.clone())
}

/// A StripeApi decorator that forwards to the REAL `StripeClient` (so requests
/// still hit the mock + get ledgered) but FAILS after the first invoice-item
/// create — simulating a crash/timeout partway through posting a creator's
/// items. The first item posts (and is ledgered by `bill_creator`); the second
/// returns an error, aborting the drive before the invoice is finalized.
struct FailAfterFirstItem {
    inner: StripeClient,
    items_seen: std::cell::Cell<usize>,
}

impl StripeApi for FailAfterFirstItem {
    async fn create_customer(&self, email: &str, creator_id: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_customer(email, creator_id).await
    }
    async fn create_checkout_setup_session(&self, c: &str, ok: &str, cancel: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_checkout_setup_session(c, ok, cancel).await
    }
    async fn create_invoice_item(
        &self,
        customer: &str,
        amount_cents: u64,
        currency: &str,
        description: &str,
        period: Period,
        idempotency_key: &str,
        lookup_key: &str,
    ) -> Result<String, zeroship_control::stripe_store::StripeError> {
        let n = self.items_seen.get();
        self.items_seen.set(n + 1);
        if n >= 1 {
            // Second (and later) item: simulate the crash/timeout window.
            return Err(zeroship_control::stripe_store::StripeError::Db(
                "simulated crash after first item".to_string(),
            ));
        }
        self.inner
            .create_invoice_item(customer, amount_cents, currency, description, period, idempotency_key, lookup_key)
            .await
    }
    async fn find_invoice_item_by_key(&self, customer: &str, lookup_key: &str) -> Result<Option<String>, zeroship_control::stripe_store::StripeError> {
        self.inner.find_invoice_item_by_key(customer, lookup_key).await
    }
    async fn create_invoice(&self, customer: &str, creator_id: &str, idempotency_key: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_invoice(customer, creator_id, idempotency_key).await
    }
    async fn finalize_invoice(&self, invoice_id: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.finalize_invoice(invoice_id).await
    }
    async fn create_meter_event(&self, event_name: &str, customer: &str, value: u64, identifier: &str, timestamp: i64) -> Result<(), zeroship_control::stripe_store::StripeError> {
        self.inner.create_meter_event(event_name, customer, value, identifier, timestamp).await
    }
    async fn meter_event_summary(&self, meter_id: &str, customer: &str, start_time: i64, end_time: i64) -> Result<u64, zeroship_control::stripe_store::StripeError> {
        self.inner.meter_event_summary(meter_id, customer, start_time, end_time).await
    }
    async fn create_connect_account(&self, email: &str, creator_id: &str, country: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_connect_account(email, creator_id, country).await
    }
    async fn create_account_link(&self, account_id: &str, refresh_url: &str, return_url: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_account_link(account_id, refresh_url, return_url).await
    }
    async fn retrieve_account(&self, account_id: &str) -> Result<zeroship_control::stripe_client::ConnectAccount, zeroship_control::stripe_store::StripeError> {
        self.inner.retrieve_account(account_id).await
    }
    async fn create_connect_payment_intent(&self, connected_account: &str, amount_cents: u64, currency: &str, application_fee_cents: u64, description: &str, idempotency_key: &str) -> Result<zeroship_control::stripe_client::ConnectPaymentIntent, zeroship_control::stripe_store::StripeError> {
        self.inner.create_connect_payment_intent(connected_account, amount_cents, currency, application_fee_cents, description, idempotency_key).await
    }
}

/// CRIT-1: a partial post then crash, followed by a re-drive AFTER Stripe's
/// Idempotency-Key window has expired (dedupe OFF). The per-app LEDGER — not
/// Stripe's 24h key — must guarantee app A's invoice item is created EXACTLY
/// ONCE; the re-drive posts ONLY app B.
///
/// RED→GREEN: without the `billing_run_items` ledger (or if the re-drive does
/// not skip ledgered apps), the second drive re-posts app A and the mock — with
/// dedupe OFF — creates a SECOND item for A (double-bill). The ledger makes it
/// GREEN: count_created == 2 total (A once + B once), never 3.
#[compio::test]
async fn partial_post_then_crash_does_not_double_bill_app_a() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "partial").await;
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "partial").await;
    let plan = make_plan(&fx.state).await;
    let app_a = make_owned_app(&fx.state, &plan, creator).await;
    let app_b = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, "cus_test_partial").await.unwrap();
    ingest_at(&fx.state, app_a, 100, period, 1).await; // 100c
    ingest_at(&fx.state, app_b, 200, period, 2).await; // 200c

    // First drive: crashes after the first invoice item posts.
    let failing = FailAfterFirstItem {
        inner: dummy_passthrough(&fx),
        items_seen: std::cell::Cell::new(0),
    };
    let res = billing_reconcile::tick_with(&fx.state, &failing, now).await;
    // The sweep swallows per-creator errors → Ok(0) (nobody fully billed), but
    // exactly ONE item must have posted + been ledgered.
    assert_eq!(res.expect("tick swallows the per-creator error"), 0, "no creator fully billed on the crashed drive");

    let created_after_crash = fx.mock.count_created("POST", "/v1/invoiceitems");
    assert_eq!(created_after_crash, 1, "exactly one item posted before the crash");
    // Claim-then-call (C1): the intent row is written BEFORE each Stripe POST, so
    // after the crash app A is CONFIRMED (non-NULL stripe_item_id) and app B is
    // INTENT-only (NULL — its POST failed). Exactly ONE confirmed post.
    let ledger_after_crash = fx
        .state
        .control_pg
        .query(
            "SELECT app_id, stripe_item_id FROM zeroship.billing_run_items WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .expect("read ledger");
    let confirmed_after_crash = ledger_after_crash
        .iter()
        .filter(|r| r.get::<_, Option<String>>("stripe_item_id").is_some())
        .count();
    assert_eq!(confirmed_after_crash, 1, "exactly one app CONFIRMED-posted after the crash (claim-then-call)");

    // Simulate >24h: Stripe's Idempotency-Key no longer dedupes.
    fx.mock.disable_dedupe();

    // Re-drive with a healthy client — the ledger must skip the already-posted
    // app and post ONLY the remaining one.
    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("re-drive tick");
    assert_eq!(billed, 1, "the creator is now fully billed on the re-drive");

    // THE guarantee: total CREATED items == 2 (A once + B once), NOT 3 — even
    // though Stripe's key window expired. The ledger, not Stripe, enforced this.
    assert_eq!(
        fx.mock.count_created("POST", "/v1/invoiceitems"),
        2,
        "no double-bill: app A's item was created exactly once across both drives (ledger guard)",
    );

    // Both apps are now ledgered, and the run is completed.
    let ledger_final = fx
        .state
        .control_pg
        .query(
            "SELECT app_id FROM zeroship.billing_run_items WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .expect("read ledger");
    assert_eq!(ledger_final.len(), 2, "both apps ledgered after the re-drive");
    let run = fx
        .state
        .control_pg
        .query(
            "SELECT stripe_invoice_id FROM zeroship.billing_runs WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .expect("read run");
    assert!(run[0].get::<_, Option<String>>("stripe_invoice_id").is_some(), "run completed");
}

/// C1 decorator: `create_invoice_item` POSTS to the real Stripe (mock) — so the
/// item EXISTS on Stripe — but then returns an Err, simulating the process
/// crashing AFTER the Stripe POST returns yet BEFORE the ledger row is confirmed
/// (its `stripe_item_id` UPDATE commits). This is the EXACT crash window the old
/// "ledger-after-call" ordering could not survive: Stripe has the item, the
/// ledger does not.
struct PostThenCrash {
    inner: StripeClient,
}

impl StripeApi for PostThenCrash {
    async fn create_customer(&self, email: &str, creator_id: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_customer(email, creator_id).await
    }
    async fn create_checkout_setup_session(&self, c: &str, ok: &str, cancel: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_checkout_setup_session(c, ok, cancel).await
    }
    async fn create_invoice_item(
        &self,
        customer: &str,
        amount_cents: u64,
        currency: &str,
        description: &str,
        period: Period,
        idempotency_key: &str,
        lookup_key: &str,
    ) -> Result<String, zeroship_control::stripe_store::StripeError> {
        // Post for real (the item lands on Stripe)…
        let _id = self
            .inner
            .create_invoice_item(customer, amount_cents, currency, description, period, idempotency_key, lookup_key)
            .await?;
        // …then "crash" before bill_creator can confirm it in the ledger.
        Err(zeroship_control::stripe_store::StripeError::Db(
            "simulated crash after the Stripe POST returned, before ledger confirm".to_string(),
        ))
    }
    async fn find_invoice_item_by_key(&self, customer: &str, lookup_key: &str) -> Result<Option<String>, zeroship_control::stripe_store::StripeError> {
        self.inner.find_invoice_item_by_key(customer, lookup_key).await
    }
    async fn create_invoice(&self, customer: &str, creator_id: &str, idempotency_key: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_invoice(customer, creator_id, idempotency_key).await
    }
    async fn finalize_invoice(&self, invoice_id: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.finalize_invoice(invoice_id).await
    }
    async fn create_meter_event(&self, event_name: &str, customer: &str, value: u64, identifier: &str, timestamp: i64) -> Result<(), zeroship_control::stripe_store::StripeError> {
        self.inner.create_meter_event(event_name, customer, value, identifier, timestamp).await
    }
    async fn meter_event_summary(&self, meter_id: &str, customer: &str, start_time: i64, end_time: i64) -> Result<u64, zeroship_control::stripe_store::StripeError> {
        self.inner.meter_event_summary(meter_id, customer, start_time, end_time).await
    }
    async fn create_connect_account(&self, email: &str, creator_id: &str, country: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_connect_account(email, creator_id, country).await
    }
    async fn create_account_link(&self, account_id: &str, refresh_url: &str, return_url: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_account_link(account_id, refresh_url, return_url).await
    }
    async fn retrieve_account(&self, account_id: &str) -> Result<zeroship_control::stripe_client::ConnectAccount, zeroship_control::stripe_store::StripeError> {
        self.inner.retrieve_account(account_id).await
    }
    async fn create_connect_payment_intent(&self, connected_account: &str, amount_cents: u64, currency: &str, application_fee_cents: u64, description: &str, idempotency_key: &str) -> Result<zeroship_control::stripe_client::ConnectPaymentIntent, zeroship_control::stripe_store::StripeError> {
        self.inner.create_connect_payment_intent(connected_account, amount_cents, currency, application_fee_cents, description, idempotency_key).await
    }
}

/// C1 (the tighter crash window): the item POSTS to Stripe, then the process
/// crashes BEFORE the ledger records the post. A re-drive AFTER Stripe's 24h
/// Idempotency-Key window has expired (dedupe OFF) must NOT post a second item —
/// the claim-then-call intent row + the deterministic metadata LOOKUP adopt the
/// already-posted item. The app's invoice item is created EXACTLY ONCE.
///
/// RED→GREEN: under the OLD ordering (ledger written AFTER the Stripe call, no
/// intent row, no zs_item_key metadata), the crashed drive leaves NO ledger row,
/// so the >24h re-drive (key expired) re-POSTs the SAME app → count_created == 2
/// (double-bill). The claim-then-call fix makes it count_created == 1.
#[compio::test]
async fn post_then_crash_before_ledger_does_not_double_bill_after_24h() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "c1crash").await;
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "c1crash").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, "cus_test_c1crash").await.unwrap();
    ingest_at(&fx.state, app, 500, period, 1).await; // 500c

    // First drive: the item posts to Stripe, then we crash before the ledger
    // confirms it.
    let crashing = PostThenCrash { inner: dummy_passthrough(&fx) };
    let res = billing_reconcile::tick_with(&fx.state, &crashing, now).await;
    assert_eq!(res.expect("sweep swallows the per-creator error"), 0, "no creator fully billed on the crashed drive");

    // The item DID post to Stripe exactly once on the crashed drive.
    assert_eq!(fx.mock.count_created("POST", "/v1/invoiceitems"), 1, "item posted once before the crash");
    // The intent row exists (claim-then-call) but is NOT yet confirmed.
    let intent = fx
        .state
        .control_pg
        .query(
            "SELECT stripe_item_id FROM zeroship.billing_run_items WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .expect("read ledger");
    assert_eq!(intent.len(), 1, "claim-then-call wrote one intent row before the POST");
    assert!(
        intent[0].get::<_, Option<String>>("stripe_item_id").is_none(),
        "the intent row's stripe_item_id is NULL (post unconfirmed at crash time)",
    );

    // Simulate >24h: Stripe's Idempotency-Key no longer dedupes.
    fx.mock.disable_dedupe();

    // Re-drive with a healthy client. The NULL intent row + the metadata lookup
    // must ADOPT the already-posted item rather than POST a duplicate.
    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("re-drive tick");
    assert_eq!(billed, 1, "the creator is fully billed on the re-drive");

    // THE guarantee: the app's invoice item was CREATED exactly once across both
    // drives — even though Stripe's key window expired. The ledger + metadata
    // lookup, not Stripe's key, enforced this.
    assert_eq!(
        fx.mock.count_created("POST", "/v1/invoiceitems"),
        1,
        "no double-bill: the item was created exactly once (claim-then-call + metadata adopt)",
    );

    // The ledger row is now confirmed and the run completed with the right amount.
    let rows = fx
        .state
        .control_pg
        .query(
            "SELECT stripe_invoice_id, amount_cents FROM zeroship.billing_runs WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .expect("read run");
    assert!(rows[0].get::<_, Option<String>>("stripe_invoice_id").is_some(), "run completed");
    assert_eq!(rows[0].get::<_, i64>("amount_cents"), 500, "billed the real amount, not $0");
    let confirmed = fx
        .state
        .control_pg
        .query(
            "SELECT stripe_item_id FROM zeroship.billing_run_items WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .expect("read ledger");
    assert_eq!(confirmed.len(), 1, "still exactly one ledger row (no duplicate)");
    assert!(confirmed[0].get::<_, Option<String>>("stripe_item_id").is_some(), "the post is now confirmed in the ledger");
}

/// A normal (≤24h) re-drive of the same crash window is STILL idempotent: with
/// dedupe ON, the re-driven POST replays Stripe's original object (no new item),
/// so the deterministic Idempotency-Key path also yields exactly one created item.
#[compio::test]
async fn post_then_crash_redrive_within_24h_is_idempotent() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "c1within").await;
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "c1within").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, "cus_test_c1within").await.unwrap();
    ingest_at(&fx.state, app, 320, period, 1).await; // 320c

    let crashing = PostThenCrash { inner: dummy_passthrough(&fx) };
    let _ = billing_reconcile::tick_with(&fx.state, &crashing, now).await;
    assert_eq!(fx.mock.count_created("POST", "/v1/invoiceitems"), 1, "item posted once before crash");

    // Re-drive WITHIN 24h: dedupe stays ON. Stripe replays the original item.
    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("re-drive within 24h");
    assert_eq!(billed, 1, "creator billed on the re-drive");
    assert_eq!(
        fx.mock.count_created("POST", "/v1/invoiceitems"),
        1,
        "no double-bill within 24h: the deterministic key replayed the original item",
    );
    let rows = fx
        .state
        .control_pg
        .query(
            "SELECT amount_cents, stripe_invoice_id FROM zeroship.billing_runs WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .expect("read run");
    assert_eq!(rows[0].get::<_, i64>("amount_cents"), 320, "billed the real amount");
    assert!(rows[0].get::<_, Option<String>>("stripe_invoice_id").is_some(), "run completed");
}

/// C2 decorator: `create_invoice` creates the draft for real (it lands on Stripe
/// carrying the swept line items) but `finalize_invoice` returns an Err — the
/// process crashes AFTER the draft is created/persisted but BEFORE finalize.
struct CrashOnFinalize {
    inner: StripeClient,
}

impl StripeApi for CrashOnFinalize {
    async fn create_customer(&self, email: &str, creator_id: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_customer(email, creator_id).await
    }
    async fn create_checkout_setup_session(&self, c: &str, ok: &str, cancel: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_checkout_setup_session(c, ok, cancel).await
    }
    async fn create_invoice_item(
        &self,
        customer: &str,
        amount_cents: u64,
        currency: &str,
        description: &str,
        period: Period,
        idempotency_key: &str,
        lookup_key: &str,
    ) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner
            .create_invoice_item(customer, amount_cents, currency, description, period, idempotency_key, lookup_key)
            .await
    }
    async fn find_invoice_item_by_key(&self, customer: &str, lookup_key: &str) -> Result<Option<String>, zeroship_control::stripe_store::StripeError> {
        self.inner.find_invoice_item_by_key(customer, lookup_key).await
    }
    async fn create_invoice(&self, customer: &str, creator_id: &str, idempotency_key: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        // Create the draft for real (it sweeps the pending items)…
        self.inner.create_invoice(customer, creator_id, idempotency_key).await
    }
    async fn finalize_invoice(&self, _invoice_id: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        // …then crash before finalize.
        Err(zeroship_control::stripe_store::StripeError::Db(
            "simulated crash after draft create, before finalize".to_string(),
        ))
    }
    async fn create_meter_event(&self, event_name: &str, customer: &str, value: u64, identifier: &str, timestamp: i64) -> Result<(), zeroship_control::stripe_store::StripeError> {
        self.inner.create_meter_event(event_name, customer, value, identifier, timestamp).await
    }
    async fn meter_event_summary(&self, meter_id: &str, customer: &str, start_time: i64, end_time: i64) -> Result<u64, zeroship_control::stripe_store::StripeError> {
        self.inner.meter_event_summary(meter_id, customer, start_time, end_time).await
    }
    async fn create_connect_account(&self, email: &str, creator_id: &str, country: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_connect_account(email, creator_id, country).await
    }
    async fn create_account_link(&self, account_id: &str, refresh_url: &str, return_url: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_account_link(account_id, refresh_url, return_url).await
    }
    async fn retrieve_account(&self, account_id: &str) -> Result<zeroship_control::stripe_client::ConnectAccount, zeroship_control::stripe_store::StripeError> {
        self.inner.retrieve_account(account_id).await
    }
    async fn create_connect_payment_intent(&self, connected_account: &str, amount_cents: u64, currency: &str, application_fee_cents: u64, description: &str, idempotency_key: &str) -> Result<zeroship_control::stripe_client::ConnectPaymentIntent, zeroship_control::stripe_store::StripeError> {
        self.inner.create_connect_payment_intent(connected_account, amount_cents, currency, application_fee_cents, description, idempotency_key).await
    }
}

/// C2 (under-bill): create draft (sweeping the real items) → crash before
/// finalize. A re-drive AFTER Stripe's 24h create-key window has expired (dedupe
/// OFF) must FINALIZE the ORIGINAL draft (which carries the items) — NOT create a
/// fresh empty draft and finalize a $0 invoice.
///
/// RED→GREEN: under the OLD non-atomic create+finalize (no persisted draft id),
/// the >24h re-drive's `create` key is expired ⇒ a NEW draft is created ⇒ it
/// sweeps NO pending items (they're on the orphaned first draft) ⇒ finalizes a $0
/// invoice (under-bill). Persisting the draft id before finalize + re-finalizing
/// THAT draft on re-drive makes the finalized invoice carry the real amount.
#[compio::test]
async fn crash_before_finalize_finalizes_original_draft_after_24h() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "c2crash").await;
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "c2crash").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, "cus_test_c2crash").await.unwrap();
    ingest_at(&fx.state, app, 700, period, 1).await; // 700c

    // First drive: items post, draft is created + persisted, then finalize crashes.
    let crashing = CrashOnFinalize { inner: dummy_passthrough(&fx) };
    let res = billing_reconcile::tick_with(&fx.state, &crashing, now).await;
    assert_eq!(res.expect("sweep swallows the per-creator error"), 0, "not fully billed (finalize crashed)");

    // The item posted, the draft was created exactly once and PERSISTED.
    assert_eq!(fx.mock.count_created("POST", "/v1/invoiceitems"), 1, "item posted once");
    assert_eq!(fx.mock.count_created_exact("POST", "/v1/invoices"), 1, "exactly one draft created (no finalize yet)");
    let after_crash = fx
        .state
        .control_pg
        .query(
            "SELECT draft_invoice_id, stripe_invoice_id FROM zeroship.billing_runs WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .expect("read run");
    let persisted_draft: Option<String> = after_crash[0].get("draft_invoice_id");
    assert!(persisted_draft.is_some(), "the draft invoice id was persisted BEFORE finalize (C2)");
    assert!(after_crash[0].get::<_, Option<String>>("stripe_invoice_id").is_none(), "not finalized yet");

    // Simulate >24h: Stripe's create Idempotency-Key window has expired.
    fx.mock.disable_dedupe();

    // Re-drive with a healthy client: it must FINALIZE the EXISTING draft, NOT
    // create a new empty one.
    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("re-drive tick");
    assert_eq!(billed, 1, "the creator is fully billed on the re-drive");

    // THE guarantee: still exactly ONE draft created across both drives (no new
    // empty draft), and the finalize targeted the ORIGINAL draft id.
    assert_eq!(
        fx.mock.count_created_exact("POST", "/v1/invoices"),
        1,
        "no new draft on re-drive: the ORIGINAL draft (with the real items) was finalized",
    );
    let finalize_req = fx
        .mock
        .requests()
        .into_iter()
        .find(|r| r.path.contains("/finalize"))
        .expect("finalize fired on re-drive");
    let draft_id = persisted_draft.unwrap();
    assert!(
        finalize_req.path.contains(&draft_id),
        "finalize targeted the persisted ORIGINAL draft id ({draft_id}); path={}",
        finalize_req.path,
    );

    // The run is completed with the REAL amount (not $0).
    let rows = fx
        .state
        .control_pg
        .query(
            "SELECT amount_cents, stripe_invoice_id FROM zeroship.billing_runs WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .expect("read run");
    assert_eq!(rows[0].get::<_, i64>("amount_cents"), 700, "finalized the real amount, NOT a $0 empty invoice");
    assert!(rows[0].get::<_, Option<String>>("stripe_invoice_id").is_some(), "run completed with the finalized invoice id");
}

// ===========================================================================
// CRIT-10: billing/setup self-service authz (own-id ok; cross-creator 403).
// ===========================================================================

/// Wire just the `billing/setup` route onto a test App (same path the prod
/// router registers).
fn billing_setup_route(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/api/creators/{id}/billing/setup")
            .route(web::post().to(zeroship_control::stripe_handlers::billing_setup)),
    );
}

/// CRIT-10: a creator may set up their OWN card (principal == :id), but a
/// creator calling billing/setup for ANOTHER creator's id is denied (the `:id`
/// is bound to the principal). A non-billing-operator principal acting on a
/// foreign id falls through to the Cedar BillingWrite gate and is FORBIDDEN.
///
/// RED→GREEN: before the fix, billing/setup gated ONLY on Cedar
/// BillingWrite/Resource::Any with `:id` unbound — so (a) self-service was
/// impossible for a normal creator (403 on their OWN id) and (b) nothing tied
/// `:id` to the principal. The fix makes own-id OK and keeps foreign-id 403.
#[compio::test]
async fn billing_setup_is_self_service_and_blocks_cross_creator() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "authz").await;

    // A normal (non-operator) creator principal. Its PAT user_id IS the creator.
    let creator_a = common::authz_fixture::non_admin_pat(&fx.state).await;
    // A second creator (a different user id) — the cross-creator target.
    let creator_b = common::authz_fixture::non_admin_pat(&fx.state).await;

    let app = test::init_service(
        web::App::new().state(fx.state.clone()).configure(billing_setup_route),
    )
    .await;

    // (1) Self-service: creator A acts on creator A's OWN id → OK (200).
    let req = test::TestRequest::post()
        .uri(&format!("/api/creators/{}/billing/setup", creator_a.user_id))
        .header("authorization", creator_a.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "creator may set up their OWN card");

    // (2) Cross-creator: creator A acts on creator B's id → FORBIDDEN (403),
    //     and no Stripe customer is created for B.
    let req = test::TestRequest::post()
        .uri(&format!("/api/creators/{}/billing/setup", creator_b.user_id))
        .header("authorization", creator_a.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a creator must NOT set up billing for a DIFFERENT creator",
    );
    let stored_b = fx.state.stripe_store.get_customer(creator_b.user_id).await.unwrap();
    assert!(stored_b.is_none(), "no customer created for the cross-creator victim");

    creator_a.cleanup(&fx.state).await;
    creator_b.cleanup(&fx.state).await;
}

/// CRIT-10 (operator path): a platform billing operator may set up ANY
/// creator's billing (foreign id) — the admin PAT carries BillingWrite.
#[compio::test]
async fn billing_setup_allows_platform_operator_for_any_creator() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "authz-op").await;

    let operator = common::authz_fixture::admin_pat(&fx.state).await;
    let creator = make_user(&fx.state, "op-target").await;

    let app = test::init_service(
        web::App::new().state(fx.state.clone()).configure(billing_setup_route),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/billing/setup"))
        .header("authorization", operator.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "a billing operator may set up any creator");

    operator.cleanup(&fx.state).await;
}

// ===========================================================================
// On-demand reconcile endpoint (POST /internal/billing/reconcile) — the
// operator-gated trigger the billing & metering E2E (tests/e2e_metering_billing.sh)
// drives so it can reconcile a chosen CLOSED period without waiting a month.
// ===========================================================================

/// Wire the `/internal/billing/reconcile` route onto a test App (same path +
/// handler the prod router registers in `main.rs`).
fn force_reconcile_route(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/internal/billing/reconcile")
            .route(web::post().to(zeroship_control::internal::force_reconcile)),
    );
}

/// The endpoint is GATED by the SAME `/internal/*` `check_auth` as every other
/// internal route (control-key bearer or `--dev-insecure`) — it is NOT an
/// unauthenticated bypass. With the fixture's `insecure_dev: false` + a
/// non-empty `control_key`:
///   * no bearer ⇒ 401 (the gate, RED if the handler skipped check_auth), and
///   * the correct control-key bearer ⇒ it drives the REAL reconcile for the
///     caller-chosen period, hitting the mock-Stripe over the wire EXACTLY once.
///
/// RED→GREEN: drop the `check_auth` call from `force_reconcile` and the
/// no-bearer request would 200 + bill — a privilege bypass. Keeping the gate
/// makes the no-bearer case 401 while the keyed case still reconciles.
#[compio::test]
async fn force_reconcile_endpoint_is_operator_gated_and_drives_a_chosen_period() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "force").await;
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "force").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, "cus_test_force").await.unwrap();
    ingest_at(&fx.state, app, 600, period, 1).await; // 600c in the CLOSED period

    let svc = test::init_service(
        web::App::new().state(fx.state.clone()).configure(force_reconcile_route),
    )
    .await;

    // (1) No bearer → 401. The gate, not the reconcile, answers.
    let req = test::TestRequest::post()
        .uri(&format!("/internal/billing/reconcile?period={now}"))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "force-reconcile without the control-key bearer must be rejected (gated, not a bypass)",
    );
    // Nothing was billed by the rejected call.
    assert_eq!(
        fx.mock.count_path("POST", "/v1/invoiceitems"),
        0,
        "the 401'd request must NOT have driven any Stripe call",
    );

    // (2) Correct control-key bearer → drives the real reconcile for `period`.
    let req = test::TestRequest::post()
        .uri(&format!("/internal/billing/reconcile?period={now}"))
        .header("authorization", "Bearer test-control-key")
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "the keyed request reconciles");

    // The mock saw exactly one invoice-item create (the owned app) + the
    // invoice create/finalize — the REAL cyper wire path, billing the chosen
    // closed period.
    assert_eq!(
        fx.mock.count_path("POST", "/v1/invoiceitems"),
        1,
        "the keyed reconcile created exactly one invoice item for the closed period",
    );

    // And it recorded a completed billing_runs row for THAT period.
    let rows = fx
        .state
        .control_pg
        .query(
            "SELECT stripe_invoice_id FROM zeroship.billing_runs \
             WHERE creator_id = $1 AND period_start = to_timestamp($2::double precision)",
            &[&creator, &(period as f64)],
        )
        .await
        .expect("read billing_runs");
    assert_eq!(rows.len(), 1, "one billing_runs row for the reconciled period");
    assert!(
        rows[0].get::<_, Option<String>>("stripe_invoice_id").is_some(),
        "the reconciled run carries a finalized invoice id",
    );
}
