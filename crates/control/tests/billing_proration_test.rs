//! Faithful integration tests for FULL usage-segment proration (billing-ops gap
//! #26, PR-4; design decision 1 OVERRIDE).
//!
//! FAITHFUL by construction (the same posture as `billing_reconcile_test`): the
//! tests drive the REAL `cyper`-based [`StripeClient`] against a localhost
//! mock-Stripe HTTP server, and the REAL `billing_reconcile::tick_with` /
//! `proration::record_plan_change` — NO shims. The plan-change write path is the
//! exact server-side code `api.rs::set_plan` runs (`record_plan_change` in a
//! per-creator-advisory-locked txn). Real Postgres via `CONTROL_TEST_DB`; silent
//! skip otherwise. The DB must have changesets 0050 + 0051 applied.

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use uuid::Uuid;

use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::cron::billing_reconcile;
use zeroship_control::metering::Metering;
use zeroship_control::proration::{self, PlanChangeOutcome, MAX_PLAN_CHANGES_PER_PERIOD};
use zeroship_control::stripe_client::StripeClient;
use zeroship_control::{
    AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_core::types::{AppUsage, UsageReport};

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

/// Serialize the reconcile-driving tests (same rationale as billing_reconcile_test:
/// `tick_with` single-flights fleet-wide via a `pg_try_advisory_lock`, so two
/// reconcile tests racing would have one LOSE the lock and skip).
static RECONCILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn tmpdir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("zs-pror-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&p).expect("mk tmpdir");
    p
}

// ===========================================================================
// Mock-Stripe HTTP server (a real localhost server the REAL cyper client hits).
// ===========================================================================

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    idempotency_key: Option<String>,
    body: String,
    replayed: bool,
}

#[derive(Default)]
struct MockState {
    requests: Vec<RecordedRequest>,
    idempotency_replies: HashMap<String, String>,
    dedupe_by_key: bool,
    invoice_items: Vec<(String, String, Option<String>)>,
    /// Stripe item ids that received a `DELETE /v1/invoiceitems/{id}` (MAJOR-1).
    deleted_items: std::collections::HashSet<String>,
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
    /// Distinct POSTs to `path_prefix` that ACTUALLY created an object (not a
    /// replayed Idempotency-Key).
    fn count_created(&self, method: &str, path_prefix: &str) -> usize {
        let st = self.state.lock().unwrap();
        st.requests
            .iter()
            .filter(|r| r.method == method && r.path.starts_with(path_prefix) && !r.replayed)
            .count()
    }
    /// The distinct invoice-item Idempotency-Keys actually used (one per segment).
    fn item_idempotency_keys(&self) -> Vec<String> {
        self.requests()
            .iter()
            .filter(|r| r.method == "POST" && r.path.starts_with("/v1/invoiceitems"))
            .filter_map(|r| r.idempotency_key.clone())
            .collect()
    }
    /// Register a PENDING invoice item on `customer` with `zs_item_key` metadata so
    /// `find_invoice_item_by_key` finds it and `delete_invoice_item` can remove it
    /// (MAJOR-1 orphan seeding). Returns the synthetic `ii_…` id.
    fn preload_invoice_item(&self, customer: &str, item_key: &str) -> String {
        let id = format!("ii_orphan_{}", short());
        self.state.lock().unwrap().invoice_items.push((
            id.clone(),
            customer.to_string(),
            Some(item_key.to_string()),
        ));
        id
    }
    /// True if `item_id` received a DELETE (MAJOR-1).
    fn was_item_deleted(&self, item_id: &str) -> bool {
        self.state.lock().unwrap().deleted_items.contains(item_id)
    }
}

async fn start_mock_stripe() -> MockStripe {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().expect("local_addr");
    let base_url = format!("http://{addr}");
    let state = Arc::new(Mutex::new(MockState {
        dedupe_by_key: true,
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
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_string();
            match key.as_str() {
                "content-length" => content_length = val.parse().unwrap_or(0),
                "idempotency-key" => idempotency_key = Some(val),
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
        RecordedRequest { method, path, idempotency_key, body, replayed: false },
        body_start + content_length,
    ))
}

fn handle_mock_request(req: &RecordedRequest, state: &Arc<Mutex<MockState>>) -> Vec<u8> {
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
    if req.method == "DELETE" && req.path.starts_with("/v1/invoiceitems/") {
        // /v1/invoiceitems/{id} — record the delete, drop it from the pending list
        // (so a later find_invoice_item_by_key no longer returns it), echo Stripe's
        // deleted-object response.
        let id = req
            .path
            .trim_start_matches("/v1/invoiceitems/")
            .split('?')
            .next()
            .unwrap_or("")
            .to_string();
        {
            let mut st = state.lock().unwrap();
            st.deleted_items.insert(id.clone());
            st.invoice_items.retain(|(iid, _, _)| iid != &id);
            st.requests.push(req.clone());
        }
        return http_200_json(&format!(r#"{{"id":"{id}","object":"invoiceitem","deleted":true}}"#));
    }
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
// Fixture.
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
        tax_provider: zeroship_control::tax::build_tax_provider(
            &zeroship_control::tax::TaxProviderConfig::native(),
        )
        .expect("native tax provider builds"),
        pairwise_salt: [0u8; 32],
    });

    Fixture { state, mock, blob_root, deploy_tmp_dir }
}

// ---------------------------------------------------------------------------
// DB seeding helpers.
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

/// Seed the global `requests` weight (1 CU/op) once.
async fn seed_weight(state: &AppState) {
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
}

/// Seed a plan with explicit base_fee / included_units / FX. FX is pico-cents/CU
/// (1e12 = 1 cent/CU). Returns the plan id.
async fn seed_plan(
    state: &AppState,
    name: &str,
    base_fee_cents: i64,
    included_units: i64,
    fx_pico: i64,
) -> String {
    let plan_id = format!("pln_{name}_{}", Uuid::new_v4().simple());
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, $2, $3, $4, $5, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', 1000000)",
            &[&plan_id, &name.to_string(), &base_fee_cents, &included_units, &fx_pico],
        )
        .await
        .expect("seed plan");
    plan_id
}

async fn make_owned_app(state: &AppState, plan_id: &str, owner: Uuid) -> Uuid {
    let name = format!("pror-{}", Uuid::new_v4());
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

/// Ingest usage at a given period_start (the CLOSED period the reconciler bills).
async fn ingest_at(state: &AppState, app: Uuid, requests: u64, period_start: i64, seq: u64) {
    let metering = Metering::new(state.registry.clone());
    let worker = format!("w-{}", Uuid::new_v4());
    metering
        .ingest_at(&report(&worker, seq, app, requests), period_start)
        .await
        .expect("ingest usage");
}

fn now_for_closed_period() -> i64 {
    chrono::Utc::now().timestamp()
}

fn prev_period(now: i64) -> i64 {
    billing_reconcile::previous_period_start_unix(now)
}

fn period_d(period_start: i64) -> chrono::NaiveDate {
    use chrono::{Datelike, TimeZone};
    let dt = chrono::Utc.timestamp_opt(period_start, 0).single().unwrap();
    chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), 1).unwrap()
}

fn dummy_passthrough(fx: &Fixture) -> StripeClient {
    StripeClient::new(SecretString::new(
        fx.state.stripe_secret_key.expose_secret().to_string(),
    ))
    .with_base_url(fx.state.stripe_base_url.clone())
}

/// Drive the REAL server-side plan-change path EXACTLY as `api.rs::set_plan` does:
/// resolve the catalog base fees server-side, then call the SHARED
/// `proration::record_plan_change_tx` (which takes the per-creator advisory lock,
/// snapshots usage server-side, flips apps.plan_id, and appends the event in one
/// txn). The `effective_at` is `now_unix` (so tests can pin the segment day). This
/// is the identical code path the handler runs — no shim.
async fn record_plan_change_like_set_plan(
    state: &AppState,
    app: Uuid,
    creator: Uuid,
    to_plan: &str,
    now_unix: i64,
) -> PlanChangeOutcome {
    let from_plan_id: Option<String> = state
        .control_pg
        .query("SELECT plan_id FROM zeroship.apps WHERE id = $1", &[&app])
        .await
        .expect("read app plan")
        .first()
        .map(|r| r.get::<_, String>("plan_id"));

    proration::record_plan_change_tx(
        &state.registry,
        &app,
        &creator,
        from_plan_id.as_deref(),
        to_plan,
        now_unix,
    )
    .await
    .expect("record_plan_change_tx")
}

/// Read all invoice-line snapshots for `(creator, app)` ordered by segment_no.
/// Returns `(segment_no, plan_id, included_units, fx, base_fee, amount, usage_snapshot)`.
#[allow(clippy::type_complexity)]
async fn read_segment_lines(
    state: &AppState,
    creator: Uuid,
    app: Uuid,
) -> Vec<(i16, String, i64, i64, i64, i64, serde_json::Value)> {
    state
        .control_pg
        .query(
            "SELECT l.segment_no, l.plan_id, l.included_units, l.fx_pico_cents_per_unit, \
                    l.base_fee_cents, l.amount_cents, l.usage_snapshot \
             FROM zeroship.invoice_lines l \
             JOIN zeroship.invoices i ON i.id = l.invoice_id \
             WHERE i.creator_id = $1 AND l.app_id = $2 \
             ORDER BY l.segment_no",
            &[&creator, &app],
        )
        .await
        .expect("read segment lines")
        .iter()
        .map(|r| {
            (
                r.get::<_, i16>("segment_no"),
                r.get::<_, String>("plan_id"),
                r.get::<_, i64>("included_units"),
                r.get::<_, i64>("fx_pico_cents_per_unit"),
                r.get::<_, i64>("base_fee_cents"),
                r.get::<_, i64>("amount_cents"),
                r.get::<_, serde_json::Value>("usage_snapshot"),
            )
        })
        .collect()
}

async fn confirmed_item_refs(state: &AppState, creator: Uuid, app: Uuid) -> i64 {
    state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.billing_line_provider_refs r \
             JOIN zeroship.invoices i ON i.id = r.invoice_id \
             WHERE i.creator_id = $1 AND r.app_id = $2 \
               AND r.provider = 'stripe' AND r.ref_kind = 'invoice_item'",
            &[&creator, &app],
        )
        .await
        .expect("count refs")[0]
        .get::<_, i64>("n")
}

async fn read_event(
    state: &AppState,
    app: Uuid,
) -> Option<(chrono::NaiveDate, serde_json::Value, Option<String>, String)> {
    state
        .control_pg
        .query(
            "SELECT period, usage_at_change, from_plan_id, to_plan_id \
             FROM zeroship.plan_change_events WHERE app_id = $1 ORDER BY effective_at DESC LIMIT 1",
            &[&app],
        )
        .await
        .expect("read event")
        .first()
        .map(|r| {
            (
                r.get::<_, chrono::NaiveDate>("period"),
                r.get::<_, serde_json::Value>("usage_at_change"),
                r.get::<_, Option<String>>("from_plan_id"),
                r.get::<_, String>("to_plan_id"),
            )
        })
}

// ===========================================================================
// Tests.
// ===========================================================================

/// (a) THE core proration test. A 2-segment app (one mid-period plan change with
/// DIFFERENT per-plan FX) posts EXACTLY 2 distinct Stripe items + 2 invoice lines,
/// each priced under ITS OWN plan's FX, segment usage = cumulative deltas. The
/// numbers are re-derived from the worked example's day-split.
///
/// RED→GREEN: against the pre-PR-4 segment-blind item key + 2-col line PK, the two
/// segments collapse to ONE Stripe item / ONE line (segment 1 under-billed).
#[compio::test]
async fn two_segment_change_with_different_fx_posts_two_items_two_lines() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "twoseg").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);
    let days_in_period = proration::days_in_period(period);

    seed_weight(&fx.state).await;
    // Free: base 0, quota 1000, fx 1 cent/CU. Pro: base 3000, quota 10000, fx 2
    // cents/CU (DIFFERENT FX — the whole reason usage-segment proration matters).
    let one_cent: i64 = 1_000_000_000_000;
    let free = seed_plan(&fx.state, "free", 0, 1_000, one_cent).await;
    let pro = seed_plan(&fx.state, "pro", 3_000, 10_000, one_cent * 2).await;

    let creator = make_user(&fx.state, "twoseg").await;
    let app = make_owned_app(&fx.state, &free, creator).await;
    fx.state
        .stripe_store
        .set_customer(creator, &format!("cus_test_twoseg_{}", Uuid::new_v4().simple()))
        .await
        .unwrap();

    // Usage at the change instant: 4000 cumulative; then upgrade to Pro on day 11.
    ingest_at(&fx.state, app, 4_000, period, 1).await;
    // Pin the change to day 11 of the CLOSED period.
    use chrono::{Datelike, TimeZone};
    let pstart = chrono::Utc.timestamp_opt(period, 0).single().unwrap();
    let day11 = chrono::Utc
        .with_ymd_and_hms(pstart.year(), pstart.month(), 11, 0, 0, 0)
        .unwrap()
        .timestamp();
    let outcome = record_plan_change_like_set_plan(&fx.state, app, creator, &pro, day11).await;
    assert!(
        matches!(outcome, PlanChangeOutcome::Recorded { .. }),
        "the change is recorded (under the cap)"
    );
    // More usage accrues under Pro: cumulative reaches 30000 by period end.
    ingest_at(&fx.state, app, 26_000, period, 2).await;

    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("tick");
    assert_eq!(billed, 1, "one creator billed");

    // EXACTLY 2 distinct Stripe invoice-items (one per segment) + EXACTLY 2 lines.
    assert_eq!(
        fx.mock.count_created("POST", "/v1/invoiceitems"),
        2,
        "two distinct Stripe items — one per segment (NOT collapsed to one)"
    );
    let mut keys = fx.mock.item_idempotency_keys();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), 2, "two DISTINCT segment-aware Idempotency-Keys");
    assert!(keys.iter().any(|k| k.ends_with(":0")), "segment 0 key");
    assert!(keys.iter().any(|k| k.ends_with(":1")), "segment 1 key");

    let lines = read_segment_lines(&fx.state, creator, app).await;
    assert_eq!(lines.len(), 2, "two invoice_lines rows (segment 0 and 1)");

    // Re-derive the worked-example numbers for a 30-day month. (The closed period
    // may be 28/29/30/31 days; derive against the ACTUAL days_in_period so the test
    // is calendar-correct. For the canonical assertions we require a 30-day month;
    // otherwise we still assert the structural invariants below.)
    let (seg0, seg1) = (&lines[0], &lines[1]);
    assert_eq!(seg0.0, 0);
    assert_eq!(seg0.1, free, "segment 0 priced under Free");
    assert_eq!(seg1.0, 1);
    assert_eq!(seg1.1, pro, "segment 1 priced under Pro");

    // Segment usage = cumulative deltas (telescopes to 30000).
    let seg0_usage: HashMap<String, i64> = serde_json::from_value(seg0.6.clone()).unwrap();
    let seg1_usage: HashMap<String, i64> = serde_json::from_value(seg1.6.clone()).unwrap();
    assert_eq!(seg0_usage.get("requests"), Some(&4_000), "seg0 delta = 4000−0");
    assert_eq!(seg1_usage.get("requests"), Some(&26_000), "seg1 delta = 30000−4000");

    // Each segment priced under ITS OWN FX (frozen on the line).
    assert_eq!(seg0.3, one_cent, "segment 0 FX = 1 cent/CU (Free)");
    assert_eq!(seg1.3, one_cent * 2, "segment 1 FX = 2 cents/CU (Pro) — per-plan FX varies");

    // The full worked-example amounts ONLY hold for a 30-day month; the closed
    // period is whatever month preceded `now`. Assert them when applicable.
    if days_in_period == 30 {
        // Free quota = round_half_up(1000×10/30)=333; Pro quota = round(10000×20/30)=6667.
        assert_eq!(seg0.2, 333, "Free pro-rated quota (10/30)");
        assert_eq!(seg1.2, 6_667, "Pro pro-rated quota (20/30)");
        assert_eq!(seg0.4, 0, "Free base fee 0");
        assert_eq!(seg1.4, 2_000, "Pro base fee 3000×20/30");
        // Free overage: max(0, 4000−333)=3667 CU × 1c = 3667.
        assert_eq!(seg0.5, 3_667, "segment 0 amount (Free, 1c/CU)");
        // Pro overage: max(0, 26000−6667)=19333 CU × 2c = 38666 + base 2000 = 40666.
        assert_eq!(seg1.5, 40_666, "segment 1 amount (Pro, 2c/CU + base) — FX-correct");
        let inv = fx
            .state
            .control_pg
            .query(
                "SELECT total_cents FROM zeroship.invoices WHERE creator_id = $1 AND period = $2::date",
                &[&creator, &period_d(period)],
            )
            .await
            .unwrap();
        assert_eq!(inv[0].get::<_, i64>("total_cents"), 3_667 + 40_666, "invoice subtotal across segments");
    }

    // Structural invariants hold for ANY month length:
    let total_days_quota_ok = seg0.4 + seg1.4 <= 3_000; // Σ base ≤ one full Pro fee
    assert!(total_days_quota_ok, "Σ prorated base ≤ one full base fee (base-fee invariant)");
}

/// (b) Invariants: Σ segment_days == days_in_period, Σ base ≤ one full fee,
/// telescoped usage == period total. Verified at the pure-function level over the
/// REAL build_segments path (faithful, no DB needed for the math identity).
#[compio::test]
async fn segment_partition_invariants_hold() {
    use chrono::TimeZone;
    use zeroship_control::pricing::{PlanPrice, FX_SCALE};
    let period = chrono::Utc
        .with_ymd_and_hms(2026, 5, 1, 0, 0, 0)
        .unwrap()
        .timestamp(); // May = 31 days
    let dim = proration::days_in_period(period);
    assert_eq!(dim, 31);

    let mut prices = HashMap::new();
    prices.insert(
        "p_a".to_string(),
        PlanPrice { base_fee_cents: 1000, included_units: 500, fx_pico_cents_per_unit: Some(FX_SCALE as u64), spend_limit_default_cents: 0 },
    );
    prices.insert(
        "p_b".to_string(),
        PlanPrice { base_fee_cents: 1000, included_units: 500, fx_pico_cents_per_unit: Some(FX_SCALE as u64), spend_limit_default_cents: 0 },
    );
    let mut end = HashMap::new();
    end.insert("requests".to_string(), 9_999i64);
    let changes = vec![proration::PlanChange {
        to_plan_id: "p_b".to_string(),
        effective_at: chrono::Utc.with_ymd_and_hms(2026, 5, 17, 0, 0, 0).unwrap(),
        usage_at_change: {
            let mut m = HashMap::new();
            m.insert("requests".to_string(), 3_000i64);
            m
        },
    }];
    let segs = proration::build_segments_with_prior(period, "p_a", &changes, &end, "p_b", &prices);

    let total_days: u32 = segs.iter().map(|s| s.end_day - s.start_day).sum();
    assert_eq!(total_days, dim, "Σ segment_days == days_in_period exactly");
    let total_base: u64 = segs.iter().map(|s| s.base_fee_cents).sum();
    assert!(total_base <= 1_000, "Σ prorated base ≤ one full fee (same plan family)");
    let total_delta: i64 = segs.iter().map(|s| *s.usage_delta.get("requests").unwrap_or(&0)).sum();
    assert_eq!(total_delta, 9_999, "telescoped usage == period total");
}

/// (c) N=0 (no plan change) ⇒ EXACTLY ONE line per app at segment_no=0, full base
/// fee + full quota + current plan — byte-for-byte the pre-PR-4 behaviour (one
/// item, one line, one provider-ref).
#[compio::test]
async fn no_change_yields_exactly_one_segment_zero_line() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "nochange").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    seed_weight(&fx.state).await;
    let one_cent: i64 = 1_000_000_000_000;
    let plan = seed_plan(&fx.state, "flat", 0, 0, one_cent).await;
    let creator = make_user(&fx.state, "nochange").await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state
        .stripe_store
        .set_customer(creator, &format!("cus_nc_{}", Uuid::new_v4().simple()))
        .await
        .unwrap();
    ingest_at(&fx.state, app, 750, period, 1).await; // 750c, no plan change

    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("tick");
    assert_eq!(billed, 1);

    assert_eq!(fx.mock.count_created("POST", "/v1/invoiceitems"), 1, "exactly one item");
    let keys = fx.mock.item_idempotency_keys();
    assert_eq!(keys.len(), 1);
    assert!(keys[0].ends_with(":0"), "single segment is segment_no 0");

    let lines = read_segment_lines(&fx.state, creator, app).await;
    assert_eq!(lines.len(), 1, "exactly one line");
    assert_eq!(lines[0].0, 0, "segment_no 0");
    assert_eq!(lines[0].1, plan, "the app's current plan");
    assert_eq!(lines[0].5, 750, "full-period charge");
    let usage: HashMap<String, i64> = serde_json::from_value(lines[0].6.clone()).unwrap();
    assert_eq!(usage.get("requests"), Some(&750), "full-period usage (delta from 0)");
    assert_eq!(confirmed_item_refs(&fx.state, creator, app).await, 1, "one provider-ref");
}

/// (f) Reconcile re-run does NOT double-post segments. After a full bill, a second
/// tick is a no-op (period finalized) and the mock saw each segment item created
/// EXACTLY once.
#[compio::test]
async fn reconcile_rerun_does_not_double_post_segments() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "rerun").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    seed_weight(&fx.state).await;
    let one_cent: i64 = 1_000_000_000_000;
    let free = seed_plan(&fx.state, "free", 0, 0, one_cent).await;
    let pro = seed_plan(&fx.state, "pro", 0, 0, one_cent).await;
    let creator = make_user(&fx.state, "rerun").await;
    let app = make_owned_app(&fx.state, &free, creator).await;
    fx.state
        .stripe_store
        .set_customer(creator, &format!("cus_rr_{}", Uuid::new_v4().simple()))
        .await
        .unwrap();
    ingest_at(&fx.state, app, 1_000, period, 1).await;
    use chrono::{Datelike, TimeZone};
    let pstart = chrono::Utc.timestamp_opt(period, 0).single().unwrap();
    let mid = chrono::Utc
        .with_ymd_and_hms(pstart.year(), pstart.month(), 15, 0, 0, 0)
        .unwrap()
        .timestamp();
    record_plan_change_like_set_plan(&fx.state, app, creator, &pro, mid).await;
    ingest_at(&fx.state, app, 2_000, period, 2).await; // cumulative 3000

    let billed1 = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("tick 1");
    assert_eq!(billed1, 1);
    let items_after_first = fx.mock.count_created("POST", "/v1/invoiceitems");
    assert_eq!(items_after_first, 2, "two segment items on the first bill");

    let billed2 = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("tick 2");
    assert_eq!(billed2, 0, "second run is a no-op (period finalized)");
    assert_eq!(
        fx.mock.count_created("POST", "/v1/invoiceitems"),
        2,
        "no double-post: each segment item created exactly once across two runs"
    );
    let lines = read_segment_lines(&fx.state, creator, app).await;
    assert_eq!(lines.len(), 2, "still exactly two lines");
}

/// (g) set_plan snapshots usage_at_change SERVER-SIDE (from usage_aggregates), with
/// server-derived frozen base fees, and a change whose effective period is already
/// FINALIZED attributes to the NEXT period.
#[compio::test]
async fn set_plan_snapshots_server_side_and_finalized_period_attributes_next() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "snap").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();

    seed_weight(&fx.state).await;
    let one_cent: i64 = 1_000_000_000_000;
    let free = seed_plan(&fx.state, "free", 0, 0, one_cent).await;
    let pro = seed_plan(&fx.state, "pro", 3_000, 0, one_cent).await;
    let creator = make_user(&fx.state, "snap").await;
    let app = make_owned_app(&fx.state, &free, creator).await;

    // CURRENT-period usage the snapshot must capture (server-side).
    let current_period = zeroship_control::metering::current_period_start_unix();
    ingest_at(&fx.state, app, 1234, current_period, 1).await;

    let outcome = record_plan_change_like_set_plan(&fx.state, app, creator, &pro, now).await;
    assert!(matches!(outcome, PlanChangeOutcome::Recorded { .. }));

    let (ev_period, usage_at_change, from_plan, to_plan) =
        read_event(&fx.state, app).await.expect("event recorded");
    let usage: HashMap<String, i64> = serde_json::from_value(usage_at_change).unwrap();
    assert_eq!(
        usage.get("requests"),
        Some(&1234),
        "usage_at_change captured the SERVER-SIDE cumulative total (not client-supplied)"
    );
    // round 4 CRITICAL-1: the row freezes NO base fee — segment pricing reads the
    // live catalog at reconcile time. The audit trail is the plan ids; the base fees
    // are recoverable from the catalog by those ids.
    assert_eq!(from_plan.as_deref(), Some(free.as_str()), "from_plan_id = Free (audit trail)");
    assert_eq!(to_plan, pro, "to_plan_id = Pro (audit trail)");
    assert_eq!(ev_period, period_d(current_period), "attributed to the current open period");

    // Now FINALIZE the current period's invoice, then change again: the new event
    // must attribute to the NEXT period (the finalized bill is frozen).
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.creator_billing (creator_id) VALUES ($1) \
             ON CONFLICT (creator_id) DO NOTHING",
            &[&creator],
        )
        .await
        .expect("ensure creator_billing row");
    let inv_id = zeroship_core::typed_id::new_invoice_id();
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.invoices (id, creator_id, period, status, subtotal_cents, \
             credit_cents, tax_cents, total_cents, finalized_at) \
             VALUES ($1, $2, $3::date, 'finalized', 0, 0, 0, 0, NOW())",
            &[&inv_id, &creator, &period_d(current_period)],
        )
        .await
        .expect("finalize current-period invoice");

    let back_to_free = record_plan_change_like_set_plan(&fx.state, app, creator, &free, now).await;
    assert!(matches!(back_to_free, PlanChangeOutcome::Recorded { period, .. }
        if period == proration::next_period_date(period_d(current_period))),
        "a change in an already-finalized period attributes to the NEXT period");
    // And apps.plan_id flipped immediately regardless of attribution.
    let cur: String = fx
        .state
        .control_pg
        .query("SELECT plan_id FROM zeroship.apps WHERE id = $1", &[&app])
        .await
        .unwrap()[0]
        .get("plan_id");
    assert_eq!(cur, free, "the plan flip applies immediately even past a finalized period");
    let _ = now;
}

/// (e) Past the per-period cap, set_plan still flips apps.plan_id but records NO
/// new snapshot, and the reconcile prices the tail under the ACTUALLY-RUNNING plan
/// (MAJOR-4) — never under a cheaper recorded plan.
#[compio::test]
async fn past_cap_flips_plan_and_tail_prices_under_running_plan() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "cap").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    seed_weight(&fx.state).await;
    let one_cent: i64 = 1_000_000_000_000;
    // Cheap plan (1c/CU) and an EXPENSIVE plan (5c/CU). The tail MUST price under
    // the expensive running plan, not the cheap last-recorded one.
    let cheap = seed_plan(&fx.state, "cheap", 0, 0, one_cent).await;
    let pricey = seed_plan(&fx.state, "pricey", 0, 0, one_cent * 5).await;
    let creator = make_user(&fx.state, "cap").await;
    let app = make_owned_app(&fx.state, &cheap, creator).await;
    fx.state
        .stripe_store
        .set_customer(creator, &format!("cus_cap_{}", Uuid::new_v4().simple()))
        .await
        .unwrap();

    // Exhaust the cap with cheap↔cheap flips (all in the closed period).
    use chrono::{Datelike, TimeZone};
    let pstart = chrono::Utc.timestamp_opt(period, 0).single().unwrap();
    for d in 2..=(2 + MAX_PLAN_CHANGES_PER_PERIOD) {
        let when = chrono::Utc
            .with_ymd_and_hms(pstart.year(), pstart.month(), d as u32, 0, 0, 0)
            .unwrap()
            .timestamp();
        let to = if d % 2 == 0 { &cheap } else { &cheap };
        record_plan_change_like_set_plan(&fx.state, app, creator, to, when).await;
    }
    // Count recorded events: capped at MAX_PLAN_CHANGES_PER_PERIOD.
    let n_events: i64 = fx
        .state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.plan_change_events \
             WHERE app_id = $1 AND period = $2::date",
            &[&app, &period_d(period)],
        )
        .await
        .unwrap()[0]
        .get("n");
    assert_eq!(n_events, MAX_PLAN_CHANGES_PER_PERIOD, "snapshots capped");

    // Now flip to the EXPENSIVE plan PAST the cap — must flip apps.plan_id but
    // record NO new snapshot.
    let late = chrono::Utc
        .with_ymd_and_hms(pstart.year(), pstart.month(), 20, 0, 0, 0)
        .unwrap()
        .timestamp();
    let outcome = record_plan_change_like_set_plan(&fx.state, app, creator, &pricey, late).await;
    assert!(
        matches!(outcome, PlanChangeOutcome::FlippedNoSnapshotCapHit),
        "past the cap: flip the plan, record no snapshot"
    );
    let n_events_after: i64 = fx
        .state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.plan_change_events \
             WHERE app_id = $1 AND period = $2::date",
            &[&app, &period_d(period)],
        )
        .await
        .unwrap()[0]
        .get("n");
    assert_eq!(n_events_after, MAX_PLAN_CHANGES_PER_PERIOD, "no new snapshot past the cap");
    let running: String = fx
        .state
        .control_pg
        .query("SELECT plan_id FROM zeroship.apps WHERE id = $1", &[&app])
        .await
        .unwrap()[0]
        .get("plan_id");
    assert_eq!(running, pricey, "apps.plan_id flipped to the expensive plan");

    // Usage AFTER the cap (the tail). Bill it: the LAST segment must price under
    // the EXPENSIVE running plan (5c/CU), not the cheap recorded one (1c/CU).
    ingest_at(&fx.state, app, 1_000, period, 1).await;
    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("tick");
    assert_eq!(billed, 1);

    let lines = read_segment_lines(&fx.state, creator, app).await;
    let last = lines.last().expect("at least one segment");
    assert_eq!(
        last.3,
        one_cent * 5,
        "the tail (last segment) prices under the ACTUALLY-RUNNING expensive FX (MAJOR-4) — \
         NOT the cheap last-recorded plan (a revenue leak)"
    );
}

/// (d) An END-missing metric floors its segment delta at max(0,…) so a vanished
/// metric never credits the bill (faithful, end-to-end through the reconcile).
#[compio::test]
async fn end_missing_metric_does_not_credit_the_bill() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "floor").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    seed_weight(&fx.state).await;
    let one_cent: i64 = 1_000_000_000_000;
    let free = seed_plan(&fx.state, "free", 0, 0, one_cent).await;
    let pro = seed_plan(&fx.state, "pro", 0, 0, one_cent).await;
    let creator = make_user(&fx.state, "floor").await;
    let app = make_owned_app(&fx.state, &free, creator).await;
    fx.state
        .stripe_store
        .set_customer(creator, &format!("cus_floor_{}", Uuid::new_v4().simple()))
        .await
        .unwrap();

    // Record a plan change whose snapshot carries a metric, then ensure the
    // period-end totals for `requests` are LOWER than the snapshot would be (we
    // simulate the END-missing case by snapshotting a high cumulative then having
    // period-end be that same value — so the post-change segment's delta is 0, not
    // negative). Concretely: ingest 5000, change plan (snapshot {requests:5000}),
    // ingest NOTHING more ⇒ period-end requests stays 5000 ⇒ seg1 delta = 0.
    ingest_at(&fx.state, app, 5_000, period, 1).await;
    use chrono::{Datelike, TimeZone};
    let pstart = chrono::Utc.timestamp_opt(period, 0).single().unwrap();
    let mid = chrono::Utc
        .with_ymd_and_hms(pstart.year(), pstart.month(), 15, 0, 0, 0)
        .unwrap()
        .timestamp();
    record_plan_change_like_set_plan(&fx.state, app, creator, &pro, mid).await;
    // No further ingest: period-end requests == 5000 (== the snapshot).

    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("tick");
    assert_eq!(billed, 1, "billed (segment 0 has the 5000 usage)");

    let lines = read_segment_lines(&fx.state, creator, app).await;
    // Segment 0 charged 5000c; segment 1's delta is 0 ⇒ $0 ⇒ NO line (skipped),
    // and crucially NO negative usage that would CREDIT the bill.
    let total: i64 = lines.iter().map(|l| l.5).sum();
    assert_eq!(total, 5_000, "segment 1's zero/floored delta never credits the bill");
    for l in &lines {
        let usage: HashMap<String, i64> = serde_json::from_value(l.6.clone()).unwrap();
        for (_m, v) in usage {
            assert!(v >= 0, "no negative usage_snapshot delta on any segment line");
        }
    }
}

/// (h) CRITICAL-1: segment pricing reads the LIVE catalog at RECONCILE time — it
/// does NOT replay off any frozen base fee on `plan_change_events` (those columns
/// were removed). Record a plan change at one plan price, then EDIT the catalog
/// (raise the base fee) BEFORE the reconcile, and assert the frozen invoice line
/// reflects the catalog value AT RECONCILE — documenting the intended (operator-
/// gated, open-period-floats-until-finalize) behaviour.
#[compio::test]
async fn segment_pricing_reflects_catalog_at_reconcile_time() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "catalogtime").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    seed_weight(&fx.state).await;
    let one_cent: i64 = 1_000_000_000_000;
    // Pro starts with base 3000; we will RAISE it to 9000 in the catalog before the
    // reconcile. Segment 1 (Pro) must price under the RECONCILE-time base (9000),
    // not the change-time value (3000) — proving pricing is NOT frozen at change.
    let free = seed_plan(&fx.state, "free", 0, 1_000, one_cent).await;
    let pro = seed_plan(&fx.state, "pro", 3_000, 10_000, one_cent).await;
    let creator = make_user(&fx.state, "catalogtime").await;
    let app = make_owned_app(&fx.state, &free, creator).await;
    fx.state
        .stripe_store
        .set_customer(creator, &format!("cus_ct_{}", Uuid::new_v4().simple()))
        .await
        .unwrap();

    ingest_at(&fx.state, app, 4_000, period, 1).await;
    use chrono::{Datelike, TimeZone};
    let pstart = chrono::Utc.timestamp_opt(period, 0).single().unwrap();
    let day11 = chrono::Utc
        .with_ymd_and_hms(pstart.year(), pstart.month(), 11, 0, 0, 0)
        .unwrap()
        .timestamp();
    record_plan_change_like_set_plan(&fx.state, app, creator, &pro, day11).await;
    ingest_at(&fx.state, app, 26_000, period, 2).await;

    // OPERATOR mid-month edit: raise Pro's base fee in the catalog AFTER the change
    // was recorded but BEFORE the reconcile. There is no per-change freeze, so the
    // open period re-prices off this new catalog value.
    fx.state
        .control_pg
        .execute(
            "UPDATE zeroship.plans SET base_fee_cents = 9000 WHERE id = $1",
            &[&pro],
        )
        .await
        .expect("raise Pro base fee");

    let days_in_period = proration::days_in_period(period);
    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("tick");
    assert_eq!(billed, 1);

    let lines = read_segment_lines(&fx.state, creator, app).await;
    let pro_line = lines.iter().find(|l| l.1 == pro).expect("a Pro segment line");
    if days_in_period == 30 {
        // Pro base day-weighted 20/30 of the RECONCILE-time fee (9000), not 3000:
        // round_half_up(9000×20/30) = 6000.
        assert_eq!(
            pro_line.4, 6_000,
            "Pro segment base reflects the RECONCILE-time catalog fee (9000×20/30), \
             NOT the change-time value (would have been 3000×20/30 = 2000)"
        );
    } else {
        // For any month length, the prorated base must EXCEED the change-time-derived
        // value, proving it tracked the catalog edit rather than a frozen snapshot.
        let change_time_derived = (3_000i64 * i64::from(days_in_period - 10)) / i64::from(days_in_period);
        assert!(
            pro_line.4 > change_time_derived,
            "Pro segment base ({}) reflects the raised reconcile-time catalog fee, not the \
             change-time value (~{})",
            pro_line.4, change_time_derived
        );
    }
}

/// (i) MAJOR-1: a re-drive that builds FEWER segments than a prior crashed drive
/// posted must NOT leave orphaned Stripe items / DB lines — else the draft sweeps
/// the stale items and the finalized subtotal disagrees with the Stripe total (an
/// over-charge). We SEED a draft with an EXTRA (orphan) segment line + its
/// provider-ref + a matching Stripe item, then drive a reconcile that builds only
/// the real (fewer) segments. Assert: the orphan Stripe item is deleted, exactly
/// the current segment set persists, and the DB subtotal == the Stripe item total.
#[compio::test]
async fn shrinking_redrive_removes_orphaned_segment_and_stripe_item() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "orphan").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    seed_weight(&fx.state).await;
    let one_cent: i64 = 1_000_000_000_000;
    // No-change app: the real build is EXACTLY one segment (segment_no 0).
    let plan = seed_plan(&fx.state, "flat", 0, 0, one_cent).await;
    let creator = make_user(&fx.state, "orphan").await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    let customer = format!("cus_orphan_{}", Uuid::new_v4().simple());
    fx.state.stripe_store.set_customer(creator, &customer).await.unwrap();
    ingest_at(&fx.state, app, 1_000, period, 1).await; // 1000c, one segment

    // SEED a crash-window draft: a draft invoice claim, a REAL segment_no=0 line,
    // AND an ORPHAN segment_no=1 line + its provider-ref, plus a matching pending
    // Stripe item registered in the mock so it is deletable by id.
    let inv_id = zeroship_core::typed_id::new_invoice_id();
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.invoices (id, creator_id, period, status) \
             VALUES ($1, $2, $3::date, 'draft')",
            &[&inv_id, &creator, &period_d(period)],
        )
        .await
        .expect("seed draft invoice");
    // The real segment 0 line (matches what the fresh build will produce).
    for (seg, amount) in [(0i16, 1_000i64), (1i16, 7_777i64)] {
        fx.state
            .control_pg
            .execute(
                "INSERT INTO zeroship.invoice_lines \
                   (invoice_id, app_id, segment_no, plan_id, included_units, \
                    fx_pico_cents_per_unit, base_fee_cents, amount_cents, \
                    usage_snapshot, weights_snapshot) \
                 VALUES ($1, $2, $3, $4, 0, $5, 0, $6, '{}'::jsonb, '{}'::jsonb)",
                &[&inv_id, &app, &seg, &plan, &one_cent, &amount],
            )
            .await
            .expect("seed line");
    }
    // The orphan segment 1 was "posted": register a Stripe item with its
    // deterministic metadata key so the orphan-reconciler can DELETE it, and a
    // provider-ref so the reconciler knows its external id directly.
    let orphan_key =
        billing_reconcile::invoice_item_idempotency_key(&creator, &app, period, 1);
    let orphan_item_id = fx.mock.preload_invoice_item(&customer, &orphan_key);
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.billing_line_provider_refs \
               (invoice_id, app_id, segment_no, provider, ref_kind, external_id) \
             VALUES ($1, $2, 1, 'stripe', 'invoice_item', $3)",
            &[&inv_id, &app, &orphan_item_id],
        )
        .await
        .expect("seed orphan provider-ref");

    // Drive the reconcile: it re-drives the existing draft, builds ONLY segment 0,
    // and must delete the orphan segment 1 (DB line + ref + Stripe item).
    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("tick");
    assert_eq!(billed, 1, "the re-driven draft finalizes");

    // The orphan Stripe item was DELETEd.
    assert!(
        fx.mock.was_item_deleted(&orphan_item_id),
        "the orphaned segment-1 Stripe item must be deleted on the shrinking re-drive"
    );

    // Exactly the current segment set (segment 0 only) persists.
    let lines = read_segment_lines(&fx.state, creator, app).await;
    assert_eq!(lines.len(), 1, "exactly one segment line persists (orphan removed)");
    assert_eq!(lines[0].0, 0, "the surviving line is segment 0");

    // DB subtotal == Stripe item total (no orphaned item left to diverge).
    let inv_total: i64 = fx
        .state
        .control_pg
        .query(
            "SELECT total_cents FROM zeroship.invoices WHERE id = $1",
            &[&inv_id],
        )
        .await
        .unwrap()[0]
        .get("total_cents");
    assert_eq!(inv_total, 1_000, "finalized subtotal is the single real segment");
    // The orphan's provider-ref is gone too.
    assert_eq!(
        confirmed_item_refs(&fx.state, creator, app).await,
        1,
        "only the surviving segment's provider-ref remains"
    );
}

/// (j) MINOR-4: a corrupt `usage_at_change` (valid JSONB but NOT a {metric: int}
/// object) must ABORT/SKIP that app's billing rather than silently becoming `{}`
/// (which would zero the segment START and massively over-count). Assert: the
/// creator is NOT billed, no Stripe item is posted, and no finalized invoice.
#[compio::test]
async fn corrupt_usage_snapshot_skips_app_instead_of_overbilling() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "corrupt").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    seed_weight(&fx.state).await;
    let one_cent: i64 = 1_000_000_000_000;
    let free = seed_plan(&fx.state, "free", 0, 0, one_cent).await;
    let pro = seed_plan(&fx.state, "pro", 0, 0, one_cent).await;
    let creator = make_user(&fx.state, "corrupt").await;
    let app = make_owned_app(&fx.state, &pro, creator).await;
    fx.state
        .stripe_store
        .set_customer(creator, &format!("cus_corrupt_{}", Uuid::new_v4().simple()))
        .await
        .unwrap();
    // Big cumulative usage so a silent {}-start would over-bill from 0.
    ingest_at(&fx.state, app, 50_000, period, 1).await;

    // Directly INSERT a plan_change_events row with a CORRUPT usage_at_change: a
    // JSON ARRAY is valid JSONB but does NOT deserialize into {metric: int}. (We
    // bypass the real write path because it only ever writes a well-formed object;
    // this simulates a poisoned/oversized snapshot at rest.)
    use chrono::{Datelike, TimeZone};
    let pstart = chrono::Utc.timestamp_opt(period, 0).single().unwrap();
    let mid = chrono::Utc
        .with_ymd_and_hms(pstart.year(), pstart.month(), 15, 0, 0, 0)
        .unwrap();
    let ev_id = zeroship_core::typed_id::new_plan_change_event_id();
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.plan_change_events \
               (id, app_id, period, from_plan_id, to_plan_id, effective_at, usage_at_change) \
             VALUES ($1, $2, $3::date, $4, $5, $6, '[1,2,3]'::jsonb)",
            &[&ev_id, &app, &period_d(period), &free, &pro, &mid],
        )
        .await
        .expect("seed corrupt event");

    // The reconcile must NOT bill this creator (the parse error aborts/skips the
    // app) — it does NOT silently price off an empty snapshot.
    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("tick returns (per-creator error is logged + skipped)");
    assert_eq!(billed, 0, "the creator with a corrupt snapshot is NOT billed (no over-bill)");
    assert_eq!(
        fx.mock.count_created("POST", "/v1/invoiceitems"),
        0,
        "no Stripe item posted for an app whose snapshot is corrupt"
    );
    let finalized: i64 = fx
        .state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.invoices \
             WHERE creator_id = $1 AND status = 'finalized'",
            &[&creator],
        )
        .await
        .unwrap()[0]
        .get("n");
    assert_eq!(finalized, 0, "no finalized invoice for the skipped creator");
}
